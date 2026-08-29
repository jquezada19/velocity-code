//! Recovery from a crash mid-`apply`/`undo`.
//!
//! `status` snapshots the repo's current shape: index epoch, file/plan
//! counts, the journal head, and whatever the journal/lock currently show.
//!
//! `doctor` inspects — and, if asked, repairs — whatever a hard crash left
//! behind: an uncommitted journal entry (`apply::apply_plan`/`undo` was
//! killed after journaling a pre-image but before the commit marker
//! landed) and/or a stale `.vc/journal/LOCK` file (the process that held
//! it died without unwinding, so `lock::Lock`'s `Drop` never ran to clean
//! it up). Three actions:
//!
//!   - `Report` — read-only, mutates nothing. Used to decide whether
//!     anything needs fixing.
//!   - `Rollback` — for every uncommitted entry, newest first, restores
//!     each of its files to the *pre*-image the entry recorded (the
//!     content as it stood before the never-completed apply/undo touched
//!     it), then deletes the entry file. Restoring an entry's files fully
//!     before deleting its entry file matters for crash-safety: if
//!     `doctor` itself is killed mid-restore, the entry file is still
//!     there, so a second `vc doctor` run finds the same uncommitted entry
//!     and re-applies the (idempotent) restore rather than losing track of
//!     it.
//!   - `Discard` — deletes uncommitted entry files without restoring
//!     anything: an explicit "keep whatever `apply`/`undo` partially
//!     wrote" choice.
//!
//! `Rollback` and `Discard` both also clear a *stale* lock (one whose
//! recorded pid is no longer running) once they're done, but refuse
//! outright — before touching anything else — if the lock's pid is still
//! alive. That refusal is the only guard against `doctor` racing a live
//! `apply`/`undo`; M1 does not give `doctor` its own exclusive lock
//! acquisition, so a live pid is treated as proof an apply is genuinely in
//! flight, not crashed.
//!
//! Deliberately does not reuse `apply::apply_plan`/`undo`: those verify a
//! plan against fresh on-disk state and route through the journal, but a
//! rollback has no plan to verify against — it's a direct, unconditional
//! restore of bytes already proven durable (they were fsynced into the
//! journal entry before the original apply/undo touched anything).
//! `apply`'s own same-directory-temp-file + `sync_all` + rename write
//! discipline isn't exported for reuse, so it's replicated here exactly
//! (`write_through`/`write_and_rename` below).

use crate::{ErrorKind, VcError, VcResult, index, journal, plan};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub struct RepoStatus {
    pub epoch8: String,
    pub files: usize,
    pub plans: usize,
    pub journal_head: Option<String>,
    pub uncommitted: Vec<String>,
    pub lock_held: bool,
}

/// Snapshot the repo: refresh the write-through index (so `epoch8`/`files`
/// reflect actual current file contents, not a possibly-stale cache), then
/// read the journal and lock state alongside it. Unlike `doctor`, `status`
/// always refreshes the index — it has no "report only, mutate nothing"
/// mode, since rebuilding `.vc/index` is exactly what makes its answer
/// trustworthy.
pub fn status(root: &Path) -> VcResult<RepoStatus> {
    let (ix, epoch) = index::refresh(root)?;
    let (_committed, uncommitted) = journal::Journal::scan(root)?;
    Ok(RepoStatus {
        epoch8: index::epoch8(&epoch).to_string(),
        files: ix.entries.len(),
        plans: count_plans(root)?,
        journal_head: journal::Journal::last_committed(root)?,
        uncommitted,
        lock_held: lock_path(root).is_file(),
    })
}

/// Count of `.vc/plans/*.json`; `0` if the directory doesn't exist yet
/// (mirrors `plan::Plan::load`'s directory-listing shape, but counts
/// instead of prefix-matching).
fn count_plans(root: &Path) -> VcResult<usize> {
    let dir = root.join(".vc/plans");
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut n = 0usize;
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            n += 1;
        }
    }
    Ok(n)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorAction {
    Report,
    Rollback,
    Discard,
}

#[derive(Debug)]
pub struct DoctorReport {
    pub rolled_back: Vec<String>,
    pub discarded: Vec<String>,
    pub lock_removed: bool,
    pub healthy: bool,
}

/// Inspect (`Report`) or repair (`Rollback`/`Discard`) whatever a crash
/// left in the journal and lock file. See the module doc comment for what
/// each action does.
pub fn doctor(root: &Path, action: DoctorAction) -> VcResult<DoctorReport> {
    match action {
        DoctorAction::Report => doctor_report(root),
        DoctorAction::Rollback => doctor_mutate(root, true),
        DoctorAction::Discard => doctor_mutate(root, false),
    }
}

/// Read-only: never writes, creates, or deletes anything.
fn doctor_report(root: &Path) -> VcResult<DoctorReport> {
    let (_committed, uncommitted) = journal::Journal::scan(root)?;
    let stale_lock = matches!(inspect_lock(root), LockState::Stale);
    Ok(DoctorReport {
        rolled_back: Vec::new(),
        discarded: Vec::new(),
        lock_removed: false,
        healthy: uncommitted.is_empty() && !stale_lock,
    })
}

/// Shared body of `Rollback` (`restore: true`) and `Discard`
/// (`restore: false`): refuse if the lock is live, otherwise walk every
/// uncommitted entry newest-first, optionally restoring its files, always
/// deleting the entry file once that entry is fully handled, then clear a
/// stale lock if one was present.
fn doctor_mutate(root: &Path, restore: bool) -> VcResult<DoctorReport> {
    // Refuse before touching anything else if the lock is held by a still
    // -running process — this check is the entire concurrency guard.
    let lock_state = refuse_if_lock_alive(root)?;

    let (_committed, uncommitted) = journal::Journal::scan(root)?;
    let mut rolled_back = Vec::new();
    let mut discarded = Vec::new();
    for id in uncommitted.iter().rev() {
        // newest (highest id) first
        if restore {
            restore_entry(root, id)?;
            rolled_back.push(id.clone());
        } else {
            discarded.push(id.clone());
        }
        // Deletion follows this entry's own restore, never a later one's:
        // if doctor is killed right here, the entry file is still on
        // disk, so a re-run finds it uncommitted again and repeats the
        // (idempotent) restore rather than forgetting about it.
        std::fs::remove_file(entry_path(root, id))?;
    }

    let lock_removed = matches!(lock_state, LockState::Stale);
    if lock_removed {
        remove_stale_lock(root)?;
    }

    Ok(DoctorReport {
        rolled_back,
        discarded,
        lock_removed,
        healthy: true,
    })
}

/// Restore every file a single uncommitted entry recorded to its
/// pre-image, via the same write-through discipline `apply` uses. Does
/// not touch the entry file itself — the caller deletes that once every
/// file here has landed.
fn restore_entry(root: &Path, id: &str) -> VcResult<()> {
    let entry = journal::Journal::load(root, id)?;
    for fi in &entry.files {
        let bytes = plan::b64d(&fi.pre_b64)?;
        write_through(root, &fi.path, &bytes)?;
    }
    Ok(())
}

fn entry_path(root: &Path, id: &str) -> PathBuf {
    root.join(".vc/journal").join(format!("{id}.json"))
}

fn lock_path(root: &Path) -> PathBuf {
    root.join(".vc/journal/LOCK")
}

enum LockState {
    /// No `.vc/journal/LOCK` file at all.
    Absent,
    /// The file exists and names a pid that's still running.
    Alive(String),
    /// The file exists but is safe to clear: its pid isn't running, or its
    /// content can't be read/parsed as one in the first place — both are
    /// treated the same way, since neither can be a live holder.
    Stale,
}

/// Classify `.vc/journal/LOCK`. Infallible by design: any failure to read
/// or interpret the file (missing, unreadable, garbage content, a `kill`
/// invocation that itself fails to run) resolves to a definite
/// `LockState` rather than propagating an `Io` error — a lock file is
/// exactly the kind of thing `doctor` exists to make sense of even when
/// it's in a bad state.
fn inspect_lock(root: &Path) -> LockState {
    let content = match std::fs::read_to_string(lock_path(root)) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return LockState::Absent,
        Err(_) => return LockState::Stale,
    };
    let pid = content.trim();
    if !pid.is_empty() && pid_is_alive(pid) {
        LockState::Alive(pid.to_string())
    } else {
        LockState::Stale
    }
}

/// `kill -0 <pid>` liveness probe (works on macOS and Linux; no `/proc` on
/// macOS, and `libc`/raw `kill(2)` isn't in the dependency allowlist).
/// Exit success means a process with that pid exists and we're allowed to
/// signal it; anything else — a nonzero exit, or `kill` failing to even
/// run — means "can't prove it's alive," which `inspect_lock` treats as
/// stale so a genuinely dead lock is never permanently unrecoverable.
fn pid_is_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .is_ok_and(|s| s.success())
}

/// The concurrency guard for `Rollback`/`Discard`: `Err(JournalBlocked)`
/// if the lock is held by a live process, otherwise the lock's state (so
/// the caller doesn't have to re-inspect it just to learn whether it was
/// present-but-stale and needs clearing).
fn refuse_if_lock_alive(root: &Path) -> VcResult<LockState> {
    let state = inspect_lock(root);
    if let LockState::Alive(pid) = &state {
        return Err(VcError::new(
            ErrorKind::JournalBlocked,
            format!("apply in progress (pid {pid})"),
        )
        .with_next("wait or kill the process"));
    }
    Ok(state)
}

fn remove_stale_lock(root: &Path) -> VcResult<()> {
    match std::fs::remove_file(lock_path(root)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Same discipline as `apply`'s private `write_through`: write into a
/// same-directory temp file, `sync_all` it, then rename over the
/// destination, so a restore is atomic and durable, never a partial/torn
/// write. Replicated rather than called: `apply`'s helper isn't exported,
/// and `doctor` restores straight from a journaled pre-image with no plan
/// involved, so it deliberately never routes through `apply_plan`/`undo`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{FileImage, Journal, JournalEntry};

    /// Brief's Step 1 failing test, verbatim.
    #[test]
    fn rollback_restores_pre_state_from_uncommitted_entry() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        std::fs::write(r.join("a.rs"), "halfway-written").unwrap();
        let e = JournalEntry {
            id: "j-000001".into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![FileImage {
                path: "a.rs".into(),
                pre_b64: crate::plan::b64e(b"original"),
                pre_hash: crate::hash::bytes_hash(b"original"),
                post_hash: crate::hash::bytes_hash(b"halfway-written"),
            }],
        };
        Journal::write_entry(&r, &e).unwrap(); // no commit marker => uncommitted
        let rep = doctor(&r, DoctorAction::Rollback).unwrap();
        assert_eq!(rep.rolled_back, vec!["j-000001"]);
        assert_eq!(std::fs::read(r.join("a.rs")).unwrap(), b"original".to_vec());
        let (c, u) = Journal::scan(&r).unwrap();
        assert!(c.is_empty() && u.is_empty());
    }

    /// Own test (a): `Report` never mutates the repo and considers a
    /// clean repo (no uncommitted entries, no lock) healthy.
    #[test]
    fn report_mutates_nothing_and_is_healthy_on_a_clean_repo() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        std::fs::write(r.join("a.rs"), "content").unwrap();

        let rep = doctor(&r, DoctorAction::Report).unwrap();
        assert!(rep.healthy);
        assert!(rep.rolled_back.is_empty());
        assert!(rep.discarded.is_empty());
        assert!(!rep.lock_removed);

        assert_eq!(std::fs::read(r.join("a.rs")).unwrap(), b"content".to_vec());
        assert!(
            !r.join(".vc/journal/LOCK").exists(),
            "Report must not create a lock file"
        );
        assert!(
            !r.join(".vc/index").exists(),
            "Report must not refresh (and thereby write) the index"
        );
    }

    /// Own test (b): `Discard` removes the uncommitted entry but leaves
    /// file content exactly as it was found (no restore).
    #[test]
    fn discard_removes_uncommitted_entry_without_touching_files() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        std::fs::write(r.join("a.rs"), "halfway-written").unwrap();
        let e = JournalEntry {
            id: "j-000001".into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![FileImage {
                path: "a.rs".into(),
                pre_b64: crate::plan::b64e(b"original"),
                pre_hash: crate::hash::bytes_hash(b"original"),
                post_hash: crate::hash::bytes_hash(b"halfway-written"),
            }],
        };
        Journal::write_entry(&r, &e).unwrap();

        let rep = doctor(&r, DoctorAction::Discard).unwrap();
        assert_eq!(rep.discarded, vec!["j-000001"]);
        assert!(rep.rolled_back.is_empty());
        assert_eq!(
            std::fs::read(r.join("a.rs")).unwrap(),
            b"halfway-written".to_vec(),
            "discard must not restore file content"
        );
        let (c, u) = Journal::scan(&r).unwrap();
        assert!(c.is_empty() && u.is_empty());
    }

    /// Own test (c): a stale lock — pid that can't possibly be running —
    /// is cleared by `Rollback`, reported via `lock_removed`.
    #[test]
    fn rollback_clears_a_stale_lock() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        std::fs::write(r.join(".vc/journal/LOCK"), "999999999").unwrap();

        let rep = doctor(&r, DoctorAction::Rollback).unwrap();
        assert!(rep.lock_removed);
        assert!(rep.rolled_back.is_empty(), "nothing uncommitted here");
        assert!(rep.healthy);
        assert!(!r.join(".vc/journal/LOCK").exists());
    }

    /// Own test (d): `status` counts a stored plan and an uncommitted
    /// journal entry correctly, and leaves `journal_head` at `None` since
    /// nothing is committed yet.
    #[test]
    fn status_counts_plans_and_uncommitted_journal_entries() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        std::fs::write(r.join("a.rs"), "one\n").unwrap();

        let reqs = vec![crate::resolve::EditRequest {
            path: "a.rs".into(),
            old: b"one".to_vec(),
            new: b"ONE".to_vec(),
            line_hint: None,
        }];
        let p = crate::plan::Plan::build(&r, crate::plan::PlanForm::Edit, &reqs).unwrap();
        p.store(&r).unwrap();

        let e = JournalEntry {
            id: "j-000001".into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![FileImage {
                path: "a.rs".into(),
                pre_b64: crate::plan::b64e(b"one\n"),
                pre_hash: crate::hash::bytes_hash(b"one\n"),
                post_hash: crate::hash::bytes_hash(b"one\n"),
            }],
        };
        Journal::write_entry(&r, &e).unwrap();

        let st = status(&r).unwrap();
        assert_eq!(st.plans, 1);
        assert_eq!(st.files, 1);
        assert_eq!(st.uncommitted, vec!["j-000001"]);
        assert_eq!(st.journal_head, None, "nothing committed yet");
        assert!(!st.lock_held);
    }

    /// Bonus: with more than one uncommitted entry, `Rollback` processes
    /// (and reports) them newest id first.
    #[test]
    fn rollback_processes_multiple_uncommitted_entries_newest_first() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        std::fs::write(r.join("a.rs"), "a-mid").unwrap();
        std::fs::write(r.join("b.rs"), "b-mid").unwrap();

        let e1 = JournalEntry {
            id: "j-000001".into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![FileImage {
                path: "a.rs".into(),
                pre_b64: crate::plan::b64e(b"a-orig"),
                pre_hash: crate::hash::bytes_hash(b"a-orig"),
                post_hash: crate::hash::bytes_hash(b"a-mid"),
            }],
        };
        let e2 = JournalEntry {
            id: "j-000002".into(),
            plan_id: "p".repeat(64),
            created_unix: 2,
            files: vec![FileImage {
                path: "b.rs".into(),
                pre_b64: crate::plan::b64e(b"b-orig"),
                pre_hash: crate::hash::bytes_hash(b"b-orig"),
                post_hash: crate::hash::bytes_hash(b"b-mid"),
            }],
        };
        Journal::write_entry(&r, &e1).unwrap();
        Journal::write_entry(&r, &e2).unwrap();

        let rep = doctor(&r, DoctorAction::Rollback).unwrap();
        assert_eq!(
            rep.rolled_back,
            vec!["j-000002", "j-000001"],
            "newest (highest id) first"
        );
        assert_eq!(std::fs::read(r.join("a.rs")).unwrap(), b"a-orig".to_vec());
        assert_eq!(std::fs::read(r.join("b.rs")).unwrap(), b"b-orig".to_vec());
    }

    /// `Rollback`/`Discard` refuse rather than touch anything when the
    /// lock names a pid that's genuinely alive right now — using this
    /// test process's own pid guarantees that.
    #[test]
    fn rollback_refuses_when_lock_pid_is_alive() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        std::fs::write(r.join(".vc/journal/LOCK"), std::process::id().to_string()).unwrap();
        std::fs::write(r.join("a.rs"), "untouched").unwrap();

        let err = doctor(&r, DoctorAction::Rollback).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::JournalBlocked));
        assert_eq!(err.exit_code(), 5);
        assert!(r.join(".vc/journal/LOCK").is_file(), "lock left in place");
        assert_eq!(
            std::fs::read(r.join("a.rs")).unwrap(),
            b"untouched".to_vec()
        );
    }
}
