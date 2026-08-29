//! The fail-closed transactional apply: preconditions, lock, patch,
//! write-through.
//!
//! `apply_plan` and `undo` are the only two entry points that mutate user
//! files, and they share the same shape: acquire the lock, verify
//! preconditions against fresh on-disk state (never the stat-cache index),
//! journal a full pre-image of everything about to change, write every
//! file through a same-directory temp file + `sync_all` + rename, mark the
//! journal entry committed, then refresh the write-through index. Any
//! failure before the commit marker lands leaves user files exactly as
//! they were found (verification failures) or leaves a recoverable,
//! uncommitted journal entry (a crash mid-write — Task 10's `vc doctor`
//! territory); `fault::point` calls mark the exact moments Task 13's
//! crash-consistency harness needs to be able to kill the process at.
//!
//! Verification reads every file's bytes exactly once: `verify_files`
//! (and `undo`'s equivalent hash-gate) hash the same bytes they hand back
//! to their caller, so there is no second, independent read later that
//! could observe a file a concurrent writer changed in between — nothing
//! is ever spliced or journaled from bytes that weren't the exact bytes
//! just verified.
//!
//! Once the commit marker is durable (`Journal::mark_committed` returns
//! `Ok`), the apply or undo **is done** — every file is written, the
//! journal entry is committed, and no later failure can undo that. So the
//! index refresh that follows the marker is not allowed to turn a real
//! success into a reported `Err`: a caller seeing `Err` would reasonably
//! assume nothing happened (and might retry, racing a fresh attempt
//! against a change that already landed). A refresh failure instead
//! degrades to `ApplyReport.warning` — the edit is exactly as good as any
//! other success, only the cached index may be stale until the next
//! `vc status` rebuilds it.

use crate::{ErrorKind, VcError, VcResult, fault, hash, index, journal, lock, plan};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct ApplyReport {
    pub journal_id: String,
    pub files: usize,
    pub edits: usize,
    pub epoch_after: String,
    /// Set only when the apply/undo itself fully succeeded (files
    /// written, journal entry committed) but the write-through index
    /// refresh that follows failed. `epoch_after` in that case is a
    /// best-effort fallback, not a freshly-recomputed epoch — the index
    /// is stale until `vc status` rebuilds it.
    pub warning: Option<String>,
}

/// Verify + patch every file named by the plan at `sha_prefix`, atomically
/// from the caller's point of view: either every edit lands and the
/// journal records it, or nothing on disk changes at all.
///
/// Order (spec §3.4): acquire lock -> load plan -> verify every named file
/// (symlink refusal, parent-dir-escape refusal, fresh-hash staleness) ->
/// assert the plan hasn't been tampered with -> journal a pre-image ->
/// write-through every file -> commit -> refresh the index.
pub fn apply_plan(root: &Path, sha_prefix: &str) -> VcResult<ApplyReport> {
    let _lock = lock::Lock::acquire(root)?;
    let p = plan::Plan::load(root, sha_prefix)?;

    // The only read of each file's content: verify_files hashes exactly
    // the bytes it hands back, so nothing below re-reads the filesystem
    // and nothing can drift between "verified" and "spliced".
    let mut verified = verify_files(root, &p)?;

    if p.edits.len() != p.expected_count {
        return Err(VcError::new(
            ErrorKind::Malformed,
            format!(
                "plan expected {} edits but has {}",
                p.expected_count,
                p.edits.len()
            ),
        ));
    }

    fault::point("pre_journal");

    // Build every touched file's full post-content in memory before
    // anything is journaled or written. Edits within a file are applied in
    // descending `start` order so earlier (lower-offset) edits' byte
    // ranges stay valid as later splices shift the tail of the buffer.
    let edits_by_file = group_edits_by_file(&p);
    let mut changes: Vec<(PathBuf, Vec<u8>, Vec<u8>)> = Vec::new();
    for (rel, edits) in &edits_by_file {
        let pre_bytes = verified.remove(*rel).ok_or_else(|| {
            VcError::new(
                ErrorKind::Malformed,
                format!("{}: no verified content for this edit", rel.display()),
            )
        })?;
        let pre_len = pre_bytes.len();
        let mut post_bytes = pre_bytes.clone();
        for e in edits.iter().rev() {
            // Edits are non-overlapping and sorted (resolve_edits already
            // guarantees this), and the fresh-hash check above already
            // proved `pre_bytes` is byte-identical to what these offsets
            // were computed against — so in the untampered case this bound
            // always holds. Checked anyway: a hand-edited plan.json could
            // carry an out-of-range offset, and `Vec::splice` panics on
            // that rather than erroring, which would be a crash, not a
            // refusal, in a tool whose whole point is failing closed.
            if e.start > e.end || e.end > pre_len {
                return Err(VcError::new(
                    ErrorKind::Malformed,
                    format!(
                        "{}: edit range {}..{} invalid for a {pre_len}-byte file",
                        rel.display(),
                        e.start,
                        e.end
                    ),
                ));
            }
            let new_bytes = plan::b64d(&e.new_b64)?;
            post_bytes.splice(e.start..e.end, new_bytes);
        }
        changes.push(((*rel).clone(), pre_bytes, post_bytes));
    }

    let files = changes.len();
    let (journal_id, epoch_after, warning) = commit_files(root, p.id(), changes)?;

    Ok(ApplyReport {
        journal_id,
        files,
        edits: p.edits.len(),
        epoch_after,
        warning,
    })
}

/// Reverse a committed journal entry: `id`, or the last committed entry if
/// `None`. Fail-closed like `apply_plan` — every file's *current* content
/// must still match the entry's recorded post-image, or the whole undo is
/// refused (`Stale`) rather than partially reverting. A successful undo
/// writes a brand-new journal entry whose pre-images are the current
/// (about-to-be-reverted) content, so undoing an undo works the same way.
pub fn undo(root: &Path, id: Option<&str>) -> VcResult<ApplyReport> {
    let _lock = lock::Lock::acquire(root)?;

    let target_id = match id {
        Some(s) => s.to_string(),
        None => journal::Journal::last_committed(root)?.ok_or_else(|| {
            VcError::new(ErrorKind::NotFound, "nothing to undo").with_next("vc status")
        })?,
    };
    let entry = journal::Journal::load(root, &target_id)?;

    // The only read of each file's content: hash exactly the bytes we're
    // about to reuse as the new journal entry's pre-image, so there is no
    // later, independent re-read that could observe something else.
    let mut current = verify_current_matches_post(root, &entry)?;

    fault::point("pre_journal");

    let mut changes: Vec<(PathBuf, Vec<u8>, Vec<u8>)> = Vec::new();
    for fi in &entry.files {
        let current_bytes = current.remove(&fi.path).ok_or_else(|| {
            VcError::new(
                ErrorKind::Malformed,
                format!("{}: no verified content for this undo", fi.path.display()),
            )
        })?;
        let restore_bytes = plan::b64d(&fi.pre_b64)?;
        changes.push((fi.path.clone(), current_bytes, restore_bytes));
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));

    let files = changes.len();
    let (journal_id, epoch_after, warning) = commit_files(root, entry.plan_id, changes)?;

    Ok(ApplyReport {
        journal_id,
        files,
        edits: 0,
        epoch_after,
        warning,
    })
}

/// Single-read hash-gate for `undo`, mirroring `verify_files`'s
/// preconditions (kernel change driven by Task 12's TOCTOU property suite —
/// see `toctou.rs`'s `undo_target_replaced_by_symlink_is_refused`): for
/// every file in `entry`, first (a) refuse to follow a symlink at the
/// tracked path — not even to hash it — and (b) refuse if its parent no
/// longer canonicalizes inside `root` (both `Toctou`, refused immediately,
/// *before* any read — a hash comparison alone can't catch a symlink whose
/// target happens to hold matching bytes). Only then (c) read its current
/// bytes exactly once and require their hash equals the journal entry's
/// recorded `post_hash` (i.e. nothing has touched the file since the
/// apply/undo being reversed) — a missing file, a read error, or a hash
/// mismatch all count as changed. Staleness from (c) is collected across
/// every file before refusing once (`Stale`), mirroring `verify_files`. On
/// success, returns the bytes just read, keyed by path, so the caller
/// never has to read the files a second time.
fn verify_current_matches_post(
    root: &Path,
    entry: &journal::JournalEntry,
) -> VcResult<BTreeMap<PathBuf, Vec<u8>>> {
    let root_real = root
        .canonicalize()
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", root.display())))?;

    let mut stale: Vec<PathBuf> = Vec::new();
    let mut current: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();

    for fi in &entry.files {
        let abs = root.join(&fi.path);

        let md = match std::fs::symlink_metadata(&abs) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                stale.push(fi.path.clone());
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if md.file_type().is_symlink() {
            return Err(VcError::new(
                ErrorKind::Toctou,
                format!("{}: refusing to follow symlink", fi.path.display()),
            ));
        }

        let parent = abs.parent().unwrap_or(root);
        let real_parent = parent
            .canonicalize()
            .map_err(|e| VcError::new(ErrorKind::Toctou, format!("{}: {e}", fi.path.display())))?;
        if !real_parent.starts_with(&root_real) {
            return Err(VcError::new(
                ErrorKind::Toctou,
                format!("{}: parent directory escaped the root", fi.path.display()),
            ));
        }

        let bytes = match std::fs::read(&abs) {
            Ok(bytes) => bytes,
            Err(_) => {
                stale.push(fi.path.clone());
                continue;
            }
        };
        if hash::bytes_hash(&bytes) == fi.post_hash {
            current.insert(fi.path.clone(), bytes);
        } else {
            stale.push(fi.path.clone());
        }
    }

    if !stale.is_empty() {
        return Err(VcError::new(
            ErrorKind::Stale,
            format!("changed since apply: {}", display_list(&stale)),
        )
        .with_next("vc status"));
    }
    Ok(current)
}

/// Verify every file the plan names, against fresh state — never the stat
/// cache — and return each verified file's content, read exactly once.
/// Three checks per file, in order:
///   (a) `symlink_metadata` refusal — never follow a symlink at the named
///       path, not even to hash it (`Toctou`, refused immediately).
///   (b) the file's canonicalized *parent* directory must still resolve
///       inside `plan.root_real` — catches an ancestor directory swapped
///       for a symlink pointing outside the repo (`Toctou`, immediate).
///   (c) a fresh hash — computed from a single `std::fs::read`, never a
///       second read and never the stat cache — must equal the plan's
///       recorded hash. Unlike (a)/(b), staleness is collected across
///       *every* file before refusing once, so the error names everything
///       that changed, not just the first file the loop happened to
///       reach. A file that has been deleted since planning counts as
///       stale too.
///
/// On success, the returned map holds exactly the bytes that were hashed
/// — the caller must build post-content from these, not from a fresh
/// read, or a file changed in the gap between verification and use would
/// be silently spliced and journaled without ever being re-checked.
fn verify_files(root: &Path, p: &plan::Plan) -> VcResult<BTreeMap<PathBuf, Vec<u8>>> {
    let mut stale: Vec<PathBuf> = Vec::new();
    let mut verified: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();

    for (rel, expected_hash) in &p.files {
        let abs = root.join(rel);

        let md = match std::fs::symlink_metadata(&abs) {
            Ok(md) => md,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                stale.push(rel.clone());
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if md.file_type().is_symlink() {
            return Err(VcError::new(
                ErrorKind::Toctou,
                format!("{}: refusing to follow symlink", rel.display()),
            ));
        }

        let parent = abs.parent().unwrap_or(root);
        let real_parent = parent
            .canonicalize()
            .map_err(|e| VcError::new(ErrorKind::Toctou, format!("{}: {e}", rel.display())))?;
        if !real_parent.starts_with(&p.root_real) {
            return Err(VcError::new(
                ErrorKind::Toctou,
                format!("{}: parent directory escaped the plan root", rel.display()),
            ));
        }

        let bytes = std::fs::read(&abs)?;
        let fresh_hash = hash::bytes_hash(&bytes);
        if &fresh_hash != expected_hash {
            stale.push(rel.clone());
            continue;
        }
        verified.insert(rel.clone(), bytes);
    }

    if !stale.is_empty() {
        return Err(VcError::new(
            ErrorKind::Stale,
            format!("changed since plan: {}", display_list(&stale)),
        )
        .with_next(format!("vc plan --refresh {}", p.sha8())));
    }
    Ok(verified)
}

/// Group `plan.edits` by path, preserving the ascending-`start` order
/// `resolve::resolve_edits` already sorted them into (so callers can apply
/// each file's edits in reverse for a descending-offset patch pass).
/// Iterating the returned map is deterministic path order, since it's a
/// `BTreeMap`.
fn group_edits_by_file(p: &plan::Plan) -> BTreeMap<&PathBuf, Vec<&plan::ResolvedEdit>> {
    let mut m: BTreeMap<&PathBuf, Vec<&plan::ResolvedEdit>> = BTreeMap::new();
    for e in &p.edits {
        m.entry(&e.path).or_default().push(e);
    }
    m
}

fn display_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The shared back half of `apply_plan` and `undo`: given the plan/entry id
/// to record and, for every touched file, its current ("pre") bytes and
/// the bytes it should become ("post"), journal a pre-image, write every
/// file through, mark the entry committed, and refresh the index.
/// `changes` must already be in deterministic (sorted-by-path) order —
/// both callers build it that way. Returns the new journal id, the
/// post-refresh epoch (or a best-effort fallback), and a warning if the
/// refresh itself failed.
///
/// Everything through `Journal::mark_committed` can still fail as an
/// ordinary `Err` — the apply/undo genuinely isn't done yet, so the
/// caller needs to see that. Once `mark_committed` returns `Ok`, though,
/// the operation *is* done, and `index::refresh` failing after that point
/// must not turn a real success into `Err` (see the module doc comment).
fn commit_files(
    root: &Path,
    plan_id: String,
    changes: Vec<(PathBuf, Vec<u8>, Vec<u8>)>,
) -> VcResult<(String, String, Option<String>)> {
    let files: Vec<journal::FileImage> = changes
        .iter()
        .map(|(path, pre, post)| journal::FileImage {
            path: path.clone(),
            pre_b64: plan::b64e(pre),
            pre_hash: hash::bytes_hash(pre),
            post_hash: hash::bytes_hash(post),
        })
        .collect();

    let journal_id = journal::Journal::next_id(root)?;
    let entry = journal::JournalEntry {
        id: journal_id.clone(),
        plan_id,
        created_unix: now_unix(),
        files,
    };
    journal::Journal::write_entry(root, &entry)?;
    fault::point("post_journal_entry");

    let multi = changes.len() > 1;
    for (i, (rel, _pre, post)) in changes.iter().enumerate() {
        write_through(root, rel, post)?;
        if i == 0 && multi {
            fault::point("mid_files");
        }
    }

    fault::point("pre_commit_marker");
    journal::Journal::mark_committed(root, &journal_id)?;
    fault::point("post_commit_marker");

    // Past this point the apply/undo is durably committed: files written,
    // journal entry marked. A refresh failure here degrades to a warning
    // instead of Err.
    let (epoch_after, warning) = match index::refresh(root) {
        Ok((_ix, epoch)) => (epoch, None),
        Err(e) => (
            stale_epoch_fingerprint(root),
            Some(format!(
                "index refresh failed: {e} — run vc status to rebuild"
            )),
        ),
    };
    Ok((journal_id, epoch_after, warning))
}

/// Best-effort fallback for `ApplyReport.epoch_after` when `index::refresh`
/// fails after the commit marker has already landed. Deliberately *not* a
/// recomputed epoch (that's exactly what just failed) — this hashes
/// whatever `.vc/index` currently holds on disk (the pre-refresh, now
/// possibly-stale index) so the field is never empty-by-coincidence with a
/// legitimate fresh epoch, or an empty string if there's nothing to read
/// (e.g. no index has ever been written). Either way it's paired with
/// `warning`, which is the actual signal telling the caller not to trust
/// it and to run `vc status`.
fn stale_epoch_fingerprint(root: &Path) -> String {
    std::fs::read(root.join(".vc/index"))
        .map(|bytes| hash::bytes_hash(&bytes))
        .unwrap_or_default()
}

/// Write `content` to `root/rel` durably and atomically: create a sibling
/// temp file (`.vc-tmp-<name>`) in the *same directory* (so the final
/// rename is same-filesystem and therefore atomic), `sync_all` it, then
/// rename it into place. The temp file is removed on a best-effort basis
/// if anything in that sequence fails.
fn write_through(root: &Path, rel: &Path, content: &[u8]) -> VcResult<()> {
    let abs = root.join(rel);
    let dir = abs.parent().unwrap_or(root);
    let file_name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".vc-tmp-{file_name}"));

    if let Err(e) = write_and_rename(&tmp, &abs, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn write_and_rename(tmp: &Path, dest: &Path, content: &[u8]) -> VcResult<()> {
    let mut f = std::fs::File::create(tmp)?;
    f.write_all(content)?;
    f.sync_all()?;
    std::fs::rename(tmp, dest)?;
    Ok(())
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve;

    fn setup_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        for (n, c) in files {
            std::fs::write(r.join(n), c).unwrap();
        }
        (d, r)
    }

    fn make_plan(root: &Path, edits: &[(&str, &str, &str)]) -> plan::Plan {
        let reqs: Vec<resolve::EditRequest> = edits
            .iter()
            .map(|(p, o, n)| resolve::EditRequest {
                path: p.into(),
                old: o.as_bytes().to_vec(),
                new: n.as_bytes().to_vec(),
                line_hint: None,
            })
            .collect();
        plan::Plan::build(root, plan::PlanForm::Edit, &reqs).unwrap()
    }

    #[test]
    fn undo_with_nothing_committed_is_not_found() {
        let (_d, r) = setup_repo(&[]);
        let err = undo(&r, None).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::NotFound));
    }

    #[test]
    fn undo_by_explicit_id_targets_that_entry_independent_of_later_entries() {
        let (_d, r) = setup_repo(&[("a.rs", "alpha\n"), ("b.rs", "bravo\n")]);

        let plan_a = make_plan(&r, &[("a.rs", "alpha", "ALPHA")]);
        let sha_a = plan_a.store(&r).unwrap();
        let rep_a = apply_plan(&r, &sha_a).unwrap();

        let plan_b = make_plan(&r, &[("b.rs", "bravo", "BRAVO")]);
        let sha_b = plan_b.store(&r).unwrap();
        apply_plan(&r, &sha_b).unwrap();

        assert_eq!(std::fs::read_to_string(r.join("a.rs")).unwrap(), "ALPHA\n");
        assert_eq!(std::fs::read_to_string(r.join("b.rs")).unwrap(), "BRAVO\n");

        undo(&r, Some(&rep_a.journal_id)).unwrap();

        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "alpha\n",
            "targeted entry undone"
        );
        assert_eq!(
            std::fs::read_to_string(r.join("b.rs")).unwrap(),
            "BRAVO\n",
            "later, untargeted entry left alone"
        );
    }

    #[test]
    fn tampered_expected_count_is_refused_as_malformed() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let mut p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        p.expected_count = 99; // simulate a hand-edited / corrupted plan file
        let sha8 = p.store(&r).unwrap();

        let err = apply_plan(&r, &sha8).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Malformed));
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "one\n",
            "refused before touching anything"
        );
    }

    /// A hand-edited plan.json could carry an edit whose byte range no
    /// longer fits the (hash-verified, otherwise-untouched) file — this
    /// must be a graceful `Malformed` refusal, not a `Vec::splice` panic.
    #[test]
    fn tampered_edit_range_is_refused_as_malformed_not_a_panic() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let mut p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        p.edits[0].end = 9_999; // far past "one\n"'s 4 bytes
        let sha8 = p.store(&r).unwrap();

        let err = apply_plan(&r, &sha8).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Malformed));
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "one\n",
            "refused before touching anything"
        );
    }

    #[test]
    fn apply_refuses_when_lock_already_held() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        let sha8 = p.store(&r).unwrap();

        let _held = lock::Lock::acquire(&r).unwrap();
        let err = apply_plan(&r, &sha8).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::JournalBlocked));
        assert_eq!(err.exit_code(), 5);
    }

    #[test]
    fn write_through_replaces_content_and_leaves_no_temp_file() {
        let (_d, r) = setup_repo(&[("a.rs", "old\n")]);
        write_through(&r, Path::new("a.rs"), b"new\n").unwrap();
        assert_eq!(std::fs::read(r.join("a.rs")).unwrap(), b"new\n");
        assert!(!r.join(".vc-tmp-a.rs").exists());
    }

    /// A post-commit `index::refresh` failure must not turn a fully
    /// committed apply into a reported `Err` — the edit already landed
    /// and the journal entry is already marked committed by the time
    /// refresh runs, so the caller must see `Ok` with a `warning`, not a
    /// failure that would make it look like nothing happened. Forces the
    /// failure by making `.vc/index` unwritable, which `index::refresh`'s
    /// final `ix.save` needs to overwrite.
    #[cfg(unix)]
    #[test]
    fn index_refresh_failure_after_commit_degrades_to_warning_not_err() {
        use std::os::unix::fs::PermissionsExt;

        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        let sha8 = p.store(&r).unwrap();

        let index_path = r.join(".vc/index");
        assert!(
            index_path.is_file(),
            "Plan::build's index::refresh already created this"
        );
        let writable = std::fs::metadata(&index_path).unwrap().permissions();
        let mut readonly = writable.clone();
        readonly.set_mode(0o400);
        std::fs::set_permissions(&index_path, readonly).unwrap();

        let result = apply_plan(&r, &sha8);

        // Restore permissions unconditionally (before any assertion can
        // panic and skip cleanup) so the tempdir can still be removed.
        std::fs::set_permissions(&index_path, writable).unwrap();

        let rep = result.expect("a post-commit refresh failure must not become Err");
        assert!(
            rep.warning.is_some(),
            "refresh failure must surface as a warning"
        );
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "ONE\n",
            "the edit itself still landed despite the refresh failure"
        );
    }
}
