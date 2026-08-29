use crate::{VcResult, hash, walk};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IndexEntry {
    pub size: u64,
    pub mtime_ns: i128,
    pub hash: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct StatIndex {
    pub entries: BTreeMap<PathBuf, IndexEntry>,
}

fn index_path(root: &Path) -> PathBuf {
    root.join(".vc/index")
}

impl StatIndex {
    pub fn load(root: &Path) -> VcResult<Option<StatIndex>> {
        let p = index_path(root);
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&p)?;
        let (ix, _) = bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .map_err(|e| crate::VcError::new(crate::ErrorKind::Malformed, format!("index: {e}")))?;
        Ok(Some(ix))
    }
    pub fn save(&self, root: &Path) -> VcResult<()> {
        std::fs::create_dir_all(root.join(".vc"))?;
        let bytes = bincode::serde::encode_to_vec(self, bincode::config::standard())
            .map_err(|e| crate::VcError::new(crate::ErrorKind::Io, e.to_string()))?;
        std::fs::write(index_path(root), bytes)?;
        Ok(())
    }
}

fn mtime_ns(md: &std::fs::Metadata) -> i128 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Walk, hash (stat fast-path against the previous index), save, return (index, epoch digest).
pub fn refresh(root: &Path) -> VcResult<(StatIndex, String)> {
    let prev = StatIndex::load(root)?.unwrap_or_default();
    let files = walk::walk_files(root)?;
    use rayon::prelude::*;
    let entries: VcResult<Vec<(PathBuf, IndexEntry)>> = files
        .par_iter()
        .map(|rel| {
            let abs = root.join(rel);
            let md = std::fs::metadata(&abs)?;
            let (size, mt) = (md.len(), mtime_ns(&md));
            let hash = match prev.entries.get(rel) {
                Some(e) if e.size == size && e.mtime_ns == mt => e.hash.clone(),
                _ => hash::file_hash(&abs)?,
            };
            Ok((
                rel.clone(),
                IndexEntry {
                    size,
                    mtime_ns: mt,
                    hash,
                },
            ))
        })
        .collect();
    let ix = StatIndex {
        entries: entries?.into_iter().collect(),
    };
    let mut h = blake3::Hasher::new();
    for (p, e) in &ix.entries {
        h.update(p.to_string_lossy().as_bytes());
        h.update(b"\0");
        h.update(e.hash.as_bytes());
        h.update(b"\n");
    }
    let epoch = h.finalize().to_hex().to_string();
    ix.save(root)?;
    Ok((ix, epoch))
}

pub fn epoch8(epoch: &str) -> &str {
    &epoch[..8.min(epoch.len())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_builds_and_epoch_changes_only_on_content_change() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "one").unwrap();
        let (ix1, e1) = refresh(r).unwrap();
        assert_eq!(ix1.entries.len(), 1);
        let (_ix2, e2) = refresh(r).unwrap();
        assert_eq!(e1, e2, "no change -> same epoch");
        std::fs::write(r.join("a.rs"), "two").unwrap();
        let (ix3, e3) = refresh(r).unwrap();
        assert_ne!(e1, e3, "content change -> new epoch");
        assert_eq!(
            ix3.entries[&PathBuf::from("a.rs")].hash,
            crate::hash::bytes_hash(b"two")
        );
    }

    #[test]
    fn stat_fast_path_reuses_hash_but_detects_touch_with_same_size() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "abc").unwrap();
        let (_, e1) = refresh(r).unwrap();
        // same size, new content, mtime will differ -> must rehash
        std::fs::write(r.join("a.rs"), "abd").unwrap();
        let (_, e2) = refresh(r).unwrap();
        assert_ne!(e1, e2);
    }
}
