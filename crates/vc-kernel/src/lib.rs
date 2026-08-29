//! velocity-code kernel

pub mod errors;
pub use errors::{ErrorKind, VcError, VcResult};

pub mod apply;
pub mod fault;
pub mod hash;
pub mod index;
pub mod journal;
pub mod lock;
pub mod plan;
pub mod recover;
pub mod resolve;
pub mod root;
pub mod walk;

/// True if `p` is safe to join onto a root: relative, and containing no
/// `..` (parent-dir) components. Shared by every kernel path that joins a
/// caller-controlled relative path (from a plan, a journal entry, or a
/// resolved edit) onto a root directory — an absolute path would silently
/// replace the root in `Path::join` instead of erroring, and a `..`
/// component can walk back out of it either way.
pub(crate) fn path_is_root_relative(p: &std::path::Path) -> bool {
    !p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

#[cfg(test)]
mod path_is_root_relative_tests {
    use super::path_is_root_relative;
    use std::path::PathBuf;

    #[test]
    fn accepts_plain_relative_paths_only() {
        assert!(path_is_root_relative(&PathBuf::from("a.rs")));
        assert!(path_is_root_relative(&PathBuf::from("sub/a.rs")));
        assert!(!path_is_root_relative(&PathBuf::from("/etc/passwd")));
        assert!(!path_is_root_relative(&PathBuf::from("../escape.rs")));
        assert!(!path_is_root_relative(&PathBuf::from(
            "sub/../../escape.rs"
        )));
    }
}
