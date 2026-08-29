use crate::{VcResult, hash, lang_tag, walk};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// On-disk magic for the versioned index header. Any file that doesn't
/// start with this (a headerless M1 file, or garbage) is treated as absent.
const INDEX_MAGIC: &[u8; 4] = b"VCIX";
/// On-disk format version. A file whose version doesn't match is treated
/// as absent — bytes are never migrated in place, only rebuilt.
const INDEX_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct IndexEntry {
    pub size: u64,
    pub mtime_ns: i128,
    pub hash: String,
    pub lang: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct StatIndex {
    pub generation: u64,
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
        if bytes.len() < 8 || &bytes[..4] != INDEX_MAGIC {
            return Ok(None);
        }
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if version != INDEX_VERSION {
            return Ok(None);
        }
        let (ix, _) = bincode::serde::decode_from_slice(&bytes[8..], bincode::config::standard())
            .map_err(|e| {
            crate::VcError::new(crate::ErrorKind::Malformed, format!("index: {e}"))
        })?;
        Ok(Some(ix))
    }
    /// Persist the index, bumping `generation` by 1 relative to the
    /// generation this struct was carrying (the generation it was loaded
    /// at — 0 for a fresh/absent index). On success, `self.generation` is
    /// updated to match what was written, so the caller's copy reflects
    /// the true on-disk state.
    pub fn save(&mut self, root: &Path) -> VcResult<()> {
        std::fs::create_dir_all(root.join(".vc"))?;
        self.generation += 1;
        let body = bincode::serde::encode_to_vec(&*self, bincode::config::standard())
            .map_err(|e| crate::VcError::new(crate::ErrorKind::Io, e.to_string()))?;
        let mut bytes = Vec::with_capacity(8 + body.len());
        bytes.extend_from_slice(INDEX_MAGIC);
        bytes.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        bytes.extend_from_slice(&body);
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
            let (hash, lang) = match prev.entries.get(rel) {
                Some(e) if e.size == size && e.mtime_ns == mt => (e.hash.clone(), e.lang.clone()),
                _ => (hash::file_hash(&abs)?, lang_tag(rel).to_string()),
            };
            Ok((
                rel.clone(),
                IndexEntry {
                    size,
                    mtime_ns: mt,
                    hash,
                    lang,
                },
            ))
        })
        .collect();
    let mut ix = StatIndex {
        generation: prev.generation,
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

    #[test]
    fn v1_headerless_index_is_treated_as_absent_and_rebuilt() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".vc")).unwrap();
        std::fs::write(d.path().join(".vc/index"), b"garbage-or-old-bincode").unwrap();
        assert!(StatIndex::load(d.path()).unwrap().is_none());
    }

    #[test]
    fn v2_roundtrip_preserves_lang_and_generation_bumps_on_save() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".vc")).unwrap();
        std::fs::write(d.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(d.path().join("b.py"), "def b(): pass\n").unwrap();
        let (ix1, _) = refresh(d.path()).unwrap();
        assert_eq!(ix1.entries[&PathBuf::from("a.rs")].lang, "rust");
        assert_eq!(ix1.entries[&PathBuf::from("b.py")].lang, "python");
        let g1 = ix1.generation;
        let (ix2, _) = refresh(d.path()).unwrap();
        assert!(ix2.generation > g1, "every save bumps generation");
    }
}
