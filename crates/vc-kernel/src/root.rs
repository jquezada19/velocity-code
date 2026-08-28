use std::path::{Path, PathBuf};

pub fn find_root(start: &Path) -> PathBuf {
    let mut cur = Some(start);
    while let Some(p) = cur {
        if p.join(".vc").is_dir() {
            return p.to_path_buf();
        }
        cur = p.parent();
    }
    start.to_path_buf()
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
        assert_eq!(find_root(&root.join("src/deep")), root);
        let elsewhere = d.path().join("other");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert_eq!(find_root(&elsewhere), elsewhere);
    }
}
