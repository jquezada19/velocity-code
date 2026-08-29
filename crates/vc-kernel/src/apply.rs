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

    verify_files(root, &p)?;

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
        let abs = root.join(rel);
        let pre_bytes = std::fs::read(&abs)?;
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
    let (journal_id, epoch_after) = commit_files(root, p.id(), changes)?;

    Ok(ApplyReport {
        journal_id,
        files,
        edits: p.edits.len(),
        epoch_after,
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

    let mut stale: Vec<PathBuf> = Vec::new();
    for fi in &entry.files {
        let abs = root.join(&fi.path);
        let matches_post = hash::file_hash(&abs).is_ok_and(|h| h == fi.post_hash);
        if !matches_post {
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

    fault::point("pre_journal");

    let mut changes: Vec<(PathBuf, Vec<u8>, Vec<u8>)> = Vec::new();
    for fi in &entry.files {
        let abs = root.join(&fi.path);
        let current_bytes = std::fs::read(&abs)?;
        let restore_bytes = plan::b64d(&fi.pre_b64)?;
        changes.push((fi.path.clone(), current_bytes, restore_bytes));
    }
    changes.sort_by(|a, b| a.0.cmp(&b.0));

    let files = changes.len();
    let (journal_id, epoch_after) = commit_files(root, entry.plan_id, changes)?;

    Ok(ApplyReport {
        journal_id,
        files,
        edits: 0,
        epoch_after,
    })
}

/// Verify every file the plan names, against fresh state — never the stat
/// cache. Three checks per file, in order:
///   (a) `symlink_metadata` refusal — never follow a symlink at the named
///       path, not even to hash it (`Toctou`, refused immediately).
///   (b) the file's canonicalized *parent* directory must still resolve
///       inside `plan.root_real` — catches an ancestor directory swapped
///       for a symlink pointing outside the repo (`Toctou`, immediate).
///   (c) a fresh `file_hash` must equal the plan's recorded hash. Unlike
///       (a)/(b), staleness is collected across *every* file before
///       refusing once, so the error names everything that changed, not
///       just the first file the loop happened to reach. A file that has
///       been deleted since planning counts as stale too.
fn verify_files(root: &Path, p: &plan::Plan) -> VcResult<()> {
    let mut stale: Vec<PathBuf> = Vec::new();

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

        let fresh_hash = hash::file_hash(&abs)?;
        if &fresh_hash != expected_hash {
            stale.push(rel.clone());
        }
    }

    if !stale.is_empty() {
        return Err(VcError::new(
            ErrorKind::Stale,
            format!("changed since plan: {}", display_list(&stale)),
        )
        .with_next(format!("vc plan --refresh {}", p.sha8())));
    }
    Ok(())
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
/// both callers build it that way. Returns the new journal id and the
/// post-refresh epoch.
fn commit_files(
    root: &Path,
    plan_id: String,
    changes: Vec<(PathBuf, Vec<u8>, Vec<u8>)>,
) -> VcResult<(String, String)> {
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

    let (_ix, epoch_after) = index::refresh(root)?;
    Ok((journal_id, epoch_after))
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
}
