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

/// Coarse language tag for a file, keyed on its extension: `"rust"` for
/// `.rs`, `"python"` for `.py`/`.pyi`, `""` otherwise. Public — vc-lang and
/// vc-query call this so the tag stays consistent with what the stat-index
/// records per `IndexEntry`.
pub fn lang_tag(p: &std::path::Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("py") | Some("pyi") => "python",
        _ => "",
    }
}

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
mod lang_tag_tests {
    use super::lang_tag;
    use std::path::Path;

    #[test]
    fn tags_known_extensions_and_falls_back_to_empty() {
        assert_eq!(lang_tag(Path::new("a.rs")), "rust");
        assert_eq!(lang_tag(Path::new("a.py")), "python");
        assert_eq!(lang_tag(Path::new("a.pyi")), "python");
        assert_eq!(lang_tag(Path::new("a.txt")), "");
        assert_eq!(lang_tag(Path::new("a")), "");
    }
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
