use crate::{ErrorKind, VcError, VcResult};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Cooperative mutual exclusion for the kernel's mutating operations
/// (`apply::apply_plan`, `apply::undo`). `acquire` creates `.vc/journal/LOCK`
/// with `create_new` (fails if it already exists) and writes the current
/// process's pid into it. The lock is released by removing that file, which
/// happens automatically on `Drop` — so a normal return (success or a
/// propagated `VcError`, since `?` still unwinds the stack) always releases
/// it. A process that dies without unwinding (killed, crashed, power loss)
/// leaves the file behind; that's the "stale lock" `vc doctor` exists to
/// clear.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    pub fn acquire(root: &Path) -> VcResult<Lock> {
        let dir = root.join(".vc/journal");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("LOCK");
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(VcError::new(
                    ErrorKind::JournalBlocked,
                    "apply in progress or stale lock",
                )
                .with_next("vc doctor"));
            }
            Err(e) => return Err(e.into()),
        };
        write!(f, "{}", std::process::id())?;
        Ok(Lock { path })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_creates_lock_file_with_pid_and_removes_on_drop() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        let lock_path = r.join(".vc/journal/LOCK");
        {
            let _lock = Lock::acquire(&r).unwrap();
            assert!(lock_path.is_file());
            let contents = std::fs::read_to_string(&lock_path).unwrap();
            assert_eq!(contents, std::process::id().to_string());
        }
        assert!(!lock_path.exists(), "lock removed on drop");
    }

    #[test]
    fn second_acquire_while_held_is_journal_blocked() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        let _lock = Lock::acquire(&r).unwrap();
        let err = Lock::acquire(&r).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::JournalBlocked));
        assert_eq!(err.exit_code(), 5);
    }

    #[test]
    fn acquire_creates_journal_dir_when_absent() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        assert!(!r.join(".vc/journal").exists());
        let _lock = Lock::acquire(&r).unwrap();
        assert!(r.join(".vc/journal").is_dir());
    }
}
