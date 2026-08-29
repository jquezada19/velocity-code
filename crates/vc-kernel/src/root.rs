use crate::{ErrorKind, VcError, VcResult};
use std::path::{Path, PathBuf};

/// Walk upward from `start` looking for the nearest ancestor holding a
/// `.vc` directory. `.vc` is checked with `symlink_metadata` (never
/// followed) at every ancestor: a symlinked `.vc` is refused (`Toctou`)
/// rather than silently treated as a valid root marker — every kernel
/// operation that follows (`Lock::acquire`, `Plan::store`, the journal)
/// writes *through* whatever `root/.vc` resolves to, so a symlinked `.vc`
/// is a way to redirect the tool's own state files to an attacker-chosen
/// location. No ancestor with a `.vc` at all is not an error — `start`
/// itself is returned, matching every command's existing "fresh repo"
/// behavior (the first `index::refresh`/`Plan::store` creates `.vc` for
/// real).
pub fn find_root(start: &Path) -> VcResult<PathBuf> {
    let mut cur = Some(start);
    while let Some(p) = cur {
        let vc = p.join(".vc");
        if let Ok(md) = std::fs::symlink_metadata(&vc) {
            if md.file_type().is_symlink() {
                return Err(VcError::new(
                    ErrorKind::Toctou,
                    format!("{}: refusing to follow symlink", vc.display()),
                ));
            }
            if md.is_dir() {
                return Ok(p.to_path_buf());
            }
            // Exists but is neither a symlink nor a directory (e.g. a
            // stray plain file named `.vc`) — not a valid root marker;
            // keep walking up, same as "doesn't exist here".
        }
        cur = p.parent();
    }
    Ok(start.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nearest_ancestor_with_vc_dir() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("repo");
        std::fs::create_dir_all(root.join(".vc")).unwrap();
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        assert_eq!(find_root(&root.join("src/deep")).unwrap(), root);
        let elsewhere = d.path().join("other");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(find_root(&elsewhere).unwrap(), elsewhere);
    }

    /// B5: a symlinked `.vc` must be refused, not silently accepted as
    /// the root marker — accepting it would make every subsequent write
    /// (lock file, journal, plans) go through the symlink to wherever it
    /// points.
    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_vc_directory() {
        let d = tempfile::tempdir().unwrap();
        let real_vc = d.path().join("real-vc");
        std::fs::create_dir_all(&real_vc).unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::os::unix::fs::symlink(&real_vc, repo.join(".vc")).unwrap();

        let err = find_root(&repo).unwrap_err();
        assert!(matches!(err.kind, ErrorKind::Toctou));
    }
}
