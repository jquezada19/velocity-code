use crate::{ErrorKind, VcError, VcResult};
use std::path::{Path, PathBuf};

pub fn walk_files(root: &Path) -> VcResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false) // include dotfiles…
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false) // deterministic across machines
        .add_custom_ignore_filename(".vcignore")
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            name != ".git" && name != ".vc"
        })
        .build();
    for entry in walker {
        let entry = entry.map_err(|e| VcError::new(ErrorKind::Io, e.to_string()))?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            let rel = entry
                .path()
                .strip_prefix(root)
                .map_err(|e| VcError::new(ErrorKind::Io, e.to_string()))?;
            out.push(rel.to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// Same ignore semantics as [`walk_files`], restricted to files under any of
/// `scope` (root-relative dirs or files). Empty scope = whole tree, identical
/// to `walk_files`. Sorted, root-relative output.
pub fn walk_scoped(root: &Path, scope: &[PathBuf]) -> VcResult<Vec<PathBuf>> {
    let all = walk_files(root)?;
    if scope.is_empty() {
        return Ok(all);
    }
    let filtered = all
        .into_iter()
        .filter(|rel| scope.iter().any(|s| rel.starts_with(s)))
        .collect();
    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_respects_gitignore_and_skips_vc_and_git() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join(".git")).unwrap();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(r.join("kept.rs"), "x").unwrap();
        std::fs::write(r.join("ignored.txt"), "x").unwrap();
        std::fs::write(r.join(".hidden"), "x").unwrap();
        std::fs::write(r.join(".git/HEAD"), "x").unwrap();
        std::fs::write(r.join(".vc/index"), "x").unwrap();
        let files = walk_files(r).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"kept.rs".to_string()));
        assert!(names.contains(&".hidden".to_string()));
        assert!(names.contains(&".gitignore".to_string()));
        assert!(!names.iter().any(|n| n.contains("ignored.txt")));
        assert!(
            !names
                .iter()
                .any(|n| n.starts_with(".git/") || n.starts_with(".vc/"))
        );
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn walk_respects_vcignore() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::write(r.join(".vcignore"), "generated.rs\n").unwrap();
        std::fs::write(r.join("kept.rs"), "x").unwrap();
        std::fs::write(r.join("generated.rs"), "x").unwrap();
        let files = walk_files(r).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"kept.rs".to_string()));
        assert!(!names.iter().any(|n| n == "generated.rs"));
    }

    #[test]
    fn walk_scoped_restricts_to_subdir_and_honors_ignores() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join(".git")).unwrap();
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(r.join("top.rs"), "x").unwrap();
        std::fs::write(r.join("sub/kept.rs"), "x").unwrap();
        std::fs::write(r.join("sub/ignored.txt"), "x").unwrap();
        let files = walk_scoped(r, &[PathBuf::from("sub")]).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["sub/kept.rs".to_string()]);
    }

    #[test]
    fn walk_scoped_empty_equals_walk_files() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join("top.rs"), "x").unwrap();
        std::fs::write(r.join("sub/kept.rs"), "x").unwrap();
        let all = walk_files(r).unwrap();
        let scoped = walk_scoped(r, &[]).unwrap();
        assert_eq!(all, scoped);
    }
}
