use crate::{ErrorKind, VcError, VcResult};
use std::path::{Path, PathBuf};

pub fn walk_files(root: &Path) -> VcResult<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false) // include dotfiles…
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false) // deterministic across machines
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
}
