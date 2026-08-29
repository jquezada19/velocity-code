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
//! An *ordinary* write failure in the same window — not a hard crash, but
//! e.g. a permission error on the Nth file's directory — lands in exactly
//! the same recoverable place: `commit_files` journals the full pre-image
//! before writing anything, so the entry it left behind is durable and
//! `vc doctor --rollback` can restore whatever was already written. The
//! error `commit_files` returns for that case says so explicitly and
//! names `vc doctor --rollback` as `next`, rather than leaving the caller
//! to guess whether the failed write left anything behind.
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
/// Order (spec §3.4, extended by the integrity wave): acquire lock ->
/// refuse if any journal entry is uncommitted (F) -> load plan -> refuse
/// if the plan was built for a different root (B2) -> verify every named
/// file (path-escape refusal (B3), symlink refusal, parent-dir-escape
/// refusal, fresh-hash staleness) -> assert the plan hasn't been
/// tampered with (edit count, per-edit offset content (C)) -> journal a
/// pre-image -> write-through every file -> commit -> refresh the index.
pub fn apply_plan(root: &Path, sha_prefix: &str) -> VcResult<ApplyReport> {
    let _lock = lock::Lock::acquire(root)?;
    refuse_if_journal_unrecovered(root)?;
    let p = plan::Plan::load(root, sha_prefix)?;

    let root_real = root
        .canonicalize()
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", root.display())))?;
    if p.root_real != root_real {
        return Err(
            VcError::new(ErrorKind::Stale, "plan was built for a different root")
                .with_next("re-plan"),
        );
    }

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
    let mut changes: Vec<(PathBuf, Vec<u8>, Vec<u8>, std::fs::Permissions)> = Vec::new();
    for (rel, edits) in &edits_by_file {
        let (pre_bytes, mode) = verified.remove(*rel).ok_or_else(|| {
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
            // C: the bytes about to be replaced must be exactly the plan's
            // own recorded `old_b64` — with the hash gate above this is
            // unreachable for an honest plan (the whole file already
            // matched its recorded hash), but a hand-tampered offset
            // (shifted, same length) would otherwise splice the WRONG
            // bytes out silently. Checked against `pre_bytes` (never
            // mutated by this loop), not `post_bytes`, so it's correct
            // regardless of splice order.
            let old_bytes = plan::b64d(&e.old_b64)?;
            if pre_bytes[e.start..e.end] != old_bytes[..] {
                return Err(VcError::new(
                    ErrorKind::Malformed,
                    "plan does not match file content",
                ));
            }
            let new_bytes = plan::b64d(&e.new_b64)?;
            post_bytes.splice(e.start..e.end, new_bytes);
        }
        changes.push(((*rel).clone(), pre_bytes, post_bytes, mode));
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
    refuse_if_journal_unrecovered(root)?;

    let target_id = match id {
        Some(s) => s.to_string(),
        None => journal::Journal::last_committed(root)?.ok_or_else(|| {
            VcError::new(ErrorKind::NotFound, "nothing to undo").with_next("vc status")
        })?,
    };
    let entry = journal::Journal::load(root, &target_id)?;
    // L: plain `undo` only ever targets a committed entry — an
    // uncommitted one is `vc doctor`'s territory. The `id.is_none()` path
    // can never reach here with an uncommitted target (`last_committed`
    // only returns committed ids), but the check is unconditional so both
    // paths share one, uniform guarantee.
    if !journal::Journal::is_committed(root, &target_id) {
        return Err(VcError::new(
            ErrorKind::JournalBlocked,
            "entry is uncommitted — recover first",
        )
        .with_next("vc doctor"));
    }

    // The only read of each file's content: hash exactly the bytes we're
    // about to reuse as the new journal entry's pre-image, so there is no
    // later, independent re-read that could observe something else.
    let mut current = verify_current_matches_post(root, &entry)?;

    fault::point("pre_journal");

    let mut changes: Vec<(PathBuf, Vec<u8>, Vec<u8>, std::fs::Permissions)> = Vec::new();
    for fi in &entry.files {
        let (current_bytes, mode) = current.remove(&fi.path).ok_or_else(|| {
            VcError::new(
                ErrorKind::Malformed,
                format!("{}: no verified content for this undo", fi.path.display()),
            )
        })?;
        let restore_bytes = plan::b64d(&fi.pre_b64)?;
        // G: the pre-image we're about to write back must itself be
        // intact — its hash must still equal what was journaled when it
        // was recorded. A hand-corrupted `pre_b64` (still valid base64,
        // wrong bytes) with `pre_hash` left untouched must refuse rather
        // than restore the wrong content.
        if hash::bytes_hash(&restore_bytes) != fi.pre_hash {
            return Err(VcError::new(
                ErrorKind::Malformed,
                "journal pre-image corrupt",
            ));
        }
        changes.push((fi.path.clone(), current_bytes, restore_bytes, mode));
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
) -> VcResult<BTreeMap<PathBuf, (Vec<u8>, std::fs::Permissions)>> {
    let root_real = root
        .canonicalize()
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", root.display())))?;

    let mut stale: Vec<PathBuf> = Vec::new();
    let mut current: BTreeMap<PathBuf, (Vec<u8>, std::fs::Permissions)> = BTreeMap::new();

    for fi in &entry.files {
        // B3: a journal path must be relative with no `..` components
        // before it's ever joined onto `root` — checked ahead of every
        // other per-file check, including the symlink/parent-escape ones
        // below, none of which are meaningful against a path that was
        // never safe to join in the first place.
        if !crate::path_is_root_relative(&fi.path) {
            return Err(VcError::new(
                ErrorKind::Malformed,
                format!(
                    "{}: journal path must be relative with no '..' components",
                    fi.path.display()
                ),
            ));
        }
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
            current.insert(fi.path.clone(), (bytes, md.permissions()));
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
fn verify_files(
    root: &Path,
    p: &plan::Plan,
) -> VcResult<BTreeMap<PathBuf, (Vec<u8>, std::fs::Permissions)>> {
    let mut stale: Vec<PathBuf> = Vec::new();
    let mut verified: BTreeMap<PathBuf, (Vec<u8>, std::fs::Permissions)> = BTreeMap::new();

    for (rel, expected_hash) in &p.files {
        // B3: a plan path must be relative with no `..` components before
        // it's ever joined onto `root` — B1 already refuses this at
        // `Plan::build` time, but this catches a plan hand-tampered after
        // being stored (B2's `root_real` check can't: `root_real` itself
        // may be untouched while one file's path was swapped).
        if !crate::path_is_root_relative(rel) {
            return Err(VcError::new(
                ErrorKind::Malformed,
                format!(
                    "{}: plan path must be relative with no '..' components",
                    rel.display()
                ),
            ));
        }
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
        verified.insert(rel.clone(), (bytes, md.permissions()));
    }

    if !stale.is_empty() {
        return Err(VcError::new(
            ErrorKind::Stale,
            format!("changed since plan: {}", display_list(&stale)),
        )
        .with_next(format!("vc plan refresh {}", p.sha8())));
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
    changes: Vec<(PathBuf, Vec<u8>, Vec<u8>, std::fs::Permissions)>,
) -> VcResult<(String, String, Option<String>)> {
    let files: Vec<journal::FileImage> = changes
        .iter()
        .map(|(path, pre, post, _mode)| journal::FileImage {
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
    for (i, (rel, _pre, post, mode)) in changes.iter().enumerate() {
        // E: past this point the transaction is durably journaled (the
        // entry above is fsynced), so a write failure here — a crash
        // (caught separately, below the process exit) or an ordinary
        // error like a permission failure on the Nth file — is never a
        // no-op the caller could safely retry from scratch. Say so, and
        // point at the exact recovery command.
        write_through(root, rel, post, mode.clone()).map_err(|e| {
            VcError::new(
                e.kind,
                format!(
                    "{}: write failed — journal entry {journal_id} is already durable: {}",
                    rel.display(),
                    e.message
                ),
            )
            .with_next("vc doctor --rollback")
        })?;
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
/// temp file in the *same directory* (so the final rename is
/// same-filesystem and therefore atomic), set its permissions to `mode`,
/// `sync_all` it, then rename it into place. The temp file is removed on
/// a best-effort basis if anything in that sequence fails.
///
/// The temp name is `.vc-tmp-<name>.<pid>.<monotonic-nanos>` — unique
/// enough that a leftover temp file from a crashed prior run, or a
/// concurrent write to the same path, essentially cannot collide with it
/// — and `write_and_rename` creates it with `create_new` rather than the
/// old plain "create" (which silently truncates whatever was already
/// there): a residual collision errors out cleanly instead of either
/// looping to find a free name or truncating another write's in-flight
/// temp file.
fn write_through(
    root: &Path,
    rel: &Path,
    content: &[u8],
    mode: std::fs::Permissions,
) -> VcResult<()> {
    let abs = root.join(rel);
    let dir = abs.parent().unwrap_or(root);
    let file_name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(
        ".vc-tmp-{file_name}.{}.{}",
        std::process::id(),
        monotonic_nanos()
    ));

    if let Err(e) = write_and_rename(&tmp, &abs, content, mode) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Nanoseconds elapsed since this process's first call — a monotonic,
/// per-process clock (never the wall clock, which can jump backward or
/// repeat across a clock adjustment). Exists solely to make
/// `write_through`'s temp file name unique.
fn monotonic_nanos() -> u128 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos()
}

fn write_and_rename(
    tmp: &Path,
    dest: &Path,
    content: &[u8],
    mode: std::fs::Permissions,
) -> VcResult<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    f.write_all(content)?;
    std::fs::set_permissions(tmp, mode)?;
    f.sync_all()?;
    std::fs::rename(tmp, dest)?;
    Ok(())
}

/// F: shared precondition for `apply_plan` and `undo`, checked right
/// after the lock is acquired — an uncommitted journal entry anywhere
/// means a prior apply/undo crashed mid-transaction, and stacking new
/// work on top of that unrecovered state is refused rather than
/// attempted. Only `vc doctor` clears this.
fn refuse_if_journal_unrecovered(root: &Path) -> VcResult<()> {
    let (_committed, uncommitted) = journal::Journal::scan(root)?;
    if !uncommitted.is_empty() {
        return Err(VcError::new(
            ErrorKind::JournalBlocked,
            "unrecovered journal entries exist",
        )
        .with_next("vc doctor"));
    }
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
        let mode = std::fs::metadata(r.join("a.rs")).unwrap().permissions();
        write_through(&r, Path::new("a.rs"), b"new\n", mode).unwrap();
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

    /// B2: a plan whose recorded `root_real` doesn't match the root
    /// `apply_plan` is actually running against — e.g. `.vc` copied or
    /// moved to a new location — must be refused as `Stale` with a
    /// specific, actionable message, not left to whatever
    /// `verify_files`'s parent-escape check happens to produce as a side
    /// effect (that check does incidentally also refuse this shape, but
    /// as a confusing `Toctou` "parent directory escaped the plan root").
    #[test]
    fn apply_refuses_when_plan_root_real_does_not_match_current_root() {
        let (_d_a, r_a) = setup_repo(&[("a.rs", "one\n")]);
        let p = make_plan(&r_a, &[("a.rs", "one", "ONE")]);
        let full_id = p.id();
        let bytes = serde_json::to_vec_pretty(&p).unwrap();

        // A different root, with the SAME plan file copied in (as if
        // `.vc` had been copied/moved) but a different canonical
        // `root_real` than the plan was built against.
        let (_d_b, r_b) = setup_repo(&[("a.rs", "one\n")]);
        std::fs::create_dir_all(r_b.join(".vc/plans")).unwrap();
        std::fs::write(
            r_b.join(".vc/plans").join(format!("{full_id}.json")),
            &bytes,
        )
        .unwrap();

        let err = apply_plan(&r_b, &p.sha8()).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Stale));
        assert_eq!(err.message, "plan was built for a different root");
        assert_eq!(
            std::fs::read_to_string(r_b.join("a.rs")).unwrap(),
            "one\n",
            "refused before touching anything"
        );
    }

    /// B3: an individual file path inside an otherwise-correctly-rooted
    /// plan must still be checked for root-escape before it's joined onto
    /// `root` — `Plan::build`'s own check (B1) only runs at build time, so
    /// a plan hand-tampered *after* being built and stored (same
    /// `root_real`, one file's key path swapped for a `..`-laden one)
    /// must be caught here, at apply time, before any join/read.
    #[test]
    fn apply_refuses_a_plan_file_path_containing_dotdot_before_joining_it() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let mut p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        let hash = p.files.remove(&PathBuf::from("a.rs")).unwrap();
        p.files.insert(PathBuf::from("../escape.rs"), hash);
        let sha8 = p.store(&r).unwrap();

        let err = apply_plan(&r, &sha8).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Malformed));
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "one\n",
            "refused before touching anything"
        );
    }

    /// C: a stored plan whose edit offset was hand-tampered (shifted, but
    /// keeping the same `end - start` length so the pre-existing
    /// out-of-range check doesn't already catch it) so the bytes at
    /// `start..end` no longer match `old_b64` must be refused as
    /// `Malformed` rather than spliced in — this is the corrupt-plan case
    /// the hash gate makes unreachable for an honest plan; the check
    /// exists to convert it from "wrong bytes silently written" into a
    /// clean refusal.
    #[test]
    fn apply_refuses_when_tampered_offset_no_longer_matches_old_b64() {
        let (_d, r) = setup_repo(&[("a.rs", "one two\n")]);
        let mut p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        // "one" sits at 0..3; shift both bounds by 4 to land on "two"
        // instead — same length, still in-bounds, wrong bytes.
        p.edits[0].start += 4;
        p.edits[0].end += 4;
        let sha8 = p.store(&r).unwrap();

        let err = apply_plan(&r, &sha8).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Malformed));
        assert_eq!(err.message, "plan does not match file content");
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "one two\n",
            "refused before touching anything"
        );
    }

    /// E: a write failure partway through `commit_files` — after the
    /// journal entry is durable, before the commit marker — must say so
    /// and point at `vc doctor --rollback`, and that recovery must
    /// actually work. Forces the failure by making the SECOND file's
    /// parent directory read-only, so the first file is already written
    /// (proving this is a genuine mid-transaction failure, not an
    /// up-front refusal) when the second file's temp-file creation fails.
    #[cfg(unix)]
    #[test]
    fn write_failure_mid_transaction_names_doctor_rollback_and_recovers() {
        use std::os::unix::fs::PermissionsExt;

        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/b.rs"), "two\n").unwrap();

        let p = make_plan(&r, &[("a.rs", "one", "ONE"), ("sub/b.rs", "two", "TWO")]);
        let sha8 = p.store(&r).unwrap();

        let sub_dir = r.join("sub");
        let writable = std::fs::metadata(&sub_dir).unwrap().permissions();
        let mut readonly = writable.clone();
        readonly.set_mode(0o500); // r-x: can't create a file inside
        std::fs::set_permissions(&sub_dir, readonly).unwrap();

        let result = apply_plan(&r, &sha8);

        // Restore permissions unconditionally before any assertion can
        // panic and skip cleanup.
        std::fs::set_permissions(&sub_dir, writable).unwrap();

        let err = result.expect_err("a write into a read-only directory must fail");
        let next = err.next.as_deref().unwrap_or_default();
        assert!(
            next.contains("vc doctor"),
            "next hint must point at vc doctor: {next:?}"
        );
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "ONE\n",
            "the first file was already written before the second file's failure"
        );

        let rep = crate::recover::doctor(&r, crate::recover::DoctorAction::Rollback).unwrap();
        assert_eq!(rep.rolled_back.len(), 1);
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "one\n",
            "doctor --rollback restores file 1 to its pre-image"
        );
    }

    /// F (apply): a journal entry left uncommitted by an earlier crash
    /// must block a brand-new `apply_plan` outright — stacking new work
    /// on top of unrecovered state is refused, not attempted.
    #[test]
    fn apply_refuses_when_an_uncommitted_journal_entry_exists() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        let sha8 = p.store(&r).unwrap();

        let uncommitted = journal::JournalEntry {
            id: "j-000001".into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![journal::FileImage {
                path: "other.rs".into(),
                pre_b64: plan::b64e(b"x"),
                pre_hash: crate::hash::bytes_hash(b"x"),
                post_hash: crate::hash::bytes_hash(b"y"),
            }],
        };
        journal::Journal::write_entry(&r, &uncommitted).unwrap(); // no marker => uncommitted

        let err = apply_plan(&r, &sha8).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::JournalBlocked));
        assert_eq!(err.exit_code(), 5);
        assert_eq!(err.message, "unrecovered journal entries exist");
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "one\n",
            "refused before touching anything"
        );
    }

    /// F (undo): the same refusal, reached through `undo` instead of
    /// `apply_plan`.
    #[test]
    fn undo_refuses_when_an_uncommitted_journal_entry_exists() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        let sha8 = p.store(&r).unwrap();
        apply_plan(&r, &sha8).unwrap(); // one committed entry to (not) undo

        let uncommitted = journal::JournalEntry {
            id: "j-000002".into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![journal::FileImage {
                path: "other.rs".into(),
                pre_b64: plan::b64e(b"x"),
                pre_hash: crate::hash::bytes_hash(b"x"),
                post_hash: crate::hash::bytes_hash(b"y"),
            }],
        };
        journal::Journal::write_entry(&r, &uncommitted).unwrap();

        let err = undo(&r, None).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::JournalBlocked));
        assert_eq!(err.exit_code(), 5);
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "ONE\n",
            "refused before touching anything"
        );
    }

    /// G (undo): a committed journal entry's `pre_b64` hand-corrupted
    /// (still valid base64, wrong bytes) without touching its recorded
    /// `pre_hash` must refuse `undo` as `Malformed` rather than restore
    /// the wrong content — mirrors `restore_entry`'s own pre-image check
    /// (see `recover.rs`'s test of the same name) for `undo`'s whole-file
    /// restore path.
    #[test]
    fn undo_refuses_when_journal_pre_image_is_corrupt() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);
        let p = make_plan(&r, &[("a.rs", "one", "ONE")]);
        let sha8 = p.store(&r).unwrap();
        let rep = apply_plan(&r, &sha8).unwrap();

        let entry_path = r
            .join(".vc/journal")
            .join(format!("{}.json", rep.journal_id));
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&entry_path).unwrap()).unwrap();
        v["files"][0]["pre_b64"] = serde_json::json!(plan::b64e(b"TAMPERED!"));
        std::fs::write(&entry_path, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

        let err = undo(&r, Some(&rep.journal_id)).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Malformed));
        assert_eq!(err.message, "journal pre-image corrupt");
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "ONE\n",
            "target untouched by the refused undo"
        );
    }

    /// H: `write_through`'s temp file name must include the pid and a
    /// monotonic counter, never just `.vc-tmp-<name>` — a file already
    /// sitting at that OLD, unqualified name must survive completely
    /// untouched (not truncated, not renamed over) because the real temp
    /// file used lands at a different, unique name.
    #[test]
    fn write_through_never_collides_with_a_preexisting_dotvc_tmp_file() {
        let (_d, r) = setup_repo(&[("a.rs", "old\n")]);
        let sentinel_path = r.join(".vc-tmp-a.rs");
        std::fs::write(&sentinel_path, b"SENTINEL-do-not-touch").unwrap();
        let mode = std::fs::metadata(r.join("a.rs")).unwrap().permissions();

        write_through(&r, Path::new("a.rs"), b"new\n", mode).unwrap();

        assert_eq!(std::fs::read(r.join("a.rs")).unwrap(), b"new\n");
        assert_eq!(
            std::fs::read(&sentinel_path).unwrap(),
            b"SENTINEL-do-not-touch".to_vec(),
            "a pre-existing file at the OLD unqualified temp name must survive untouched"
        );
        let leftover: Vec<String> = std::fs::read_dir(&r)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".vc-tmp-") && n != ".vc-tmp-a.rs")
            .collect();
        assert!(
            leftover.is_empty(),
            "no stray temp file should remain: {leftover:?}"
        );
    }

    /// I: `apply`/`undo` must not silently change a file's permission
    /// bits as a side effect of rewriting its content — the temp file's
    /// mode is set to match the original before it's renamed into place,
    /// both on the way out (`apply`) and on the way back (`undo`).
    #[cfg(unix)]
    #[test]
    fn apply_and_undo_preserve_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let (_d, r) = setup_repo(&[("script.sh", "#!/bin/sh\necho old\n")]);
        let mut perms = std::fs::metadata(r.join("script.sh"))
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(r.join("script.sh"), perms).unwrap();

        let p = make_plan(&r, &[("script.sh", "old", "new")]);
        let sha8 = p.store(&r).unwrap();
        apply_plan(&r, &sha8).unwrap();
        assert_eq!(
            std::fs::metadata(r.join("script.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "mode preserved after apply"
        );

        undo(&r, None).unwrap();
        assert_eq!(
            std::fs::metadata(r.join("script.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "mode preserved after undo"
        );
    }

    /// L: `undo` targeting an entry by explicit id that lacks its
    /// `.committed` marker must refuse — doctor, not plain `undo`, owns
    /// uncommitted entries. (In this exact scenario F's unconditional
    /// "any uncommitted entry blocks everything" check necessarily fires
    /// first, since the target itself being uncommitted already satisfies
    /// F's condition — see the task report for the ordering discussion.
    /// Both checks are independently implemented per spec; L's own
    /// `Journal::is_committed` predicate has its own direct unit test in
    /// `journal.rs`.)
    #[test]
    fn undo_by_explicit_id_refuses_an_uncommitted_entry() {
        let (_d, r) = setup_repo(&[("a.rs", "one\n")]);

        let uncommitted = journal::JournalEntry {
            id: "j-000001".into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![journal::FileImage {
                path: "a.rs".into(),
                pre_b64: plan::b64e(b"one\n"),
                pre_hash: crate::hash::bytes_hash(b"one\n"),
                post_hash: crate::hash::bytes_hash(b"one\n"),
            }],
        };
        journal::Journal::write_entry(&r, &uncommitted).unwrap(); // no marker

        let err = undo(&r, Some("j-000001")).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::JournalBlocked));
        assert_eq!(
            std::fs::read_to_string(r.join("a.rs")).unwrap(),
            "one\n",
            "refused before touching anything"
        );
    }
}
