use crate::{VcResult, hash, journal::fsync_dir, lang_tag, walk};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write as _;
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
    ///
    /// Every read verb (`query`/`outline`/`read`) calls `refresh`, which
    /// calls this — so concurrent `vc` invocations in one repo can call
    /// `save` at the same time. Writes go to a uniquely-named temp sibling
    /// in `.vc/` first (created with `create_new`, so a name collision
    /// fails and retries rather than truncating another writer's temp file
    /// — see [`create_temp_file`]), fsynced, then `rename`d into place (same
    /// write-temp-then-rename discipline as `journal::write_entry`): a
    /// rename onto an existing path is atomic on the same filesystem, so
    /// `StatIndex::load` can never observe a torn (valid-header,
    /// truncated/interleaved-body) file the way a bare `std::fs::write`
    /// (truncate + write in place) could. Concurrent writers may still
    /// race on *which* rename lands last — that's fine, the index is a
    /// pure cache; what must never happen is a reader seeing a
    /// partially-written one.
    pub fn save(&mut self, root: &Path) -> VcResult<()> {
        let dir = root.join(".vc");
        std::fs::create_dir_all(&dir)?;
        self.generation += 1;
        let body = bincode::serde::encode_to_vec(&*self, bincode::config::standard())
            .map_err(|e| crate::VcError::new(crate::ErrorKind::Io, e.to_string()))?;
        let mut bytes = Vec::with_capacity(8 + body.len());
        bytes.extend_from_slice(INDEX_MAGIC);
        bytes.extend_from_slice(&INDEX_VERSION.to_le_bytes());
        bytes.extend_from_slice(&body);

        let (tmp, mut f) = create_temp_file(&dir)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, index_path(root))?;
        fsync_dir(&dir)?;
        Ok(())
    }
}

/// How many suffixes [`create_temp_file`] will try before giving up.
/// A collision needs two writers to pick the same pid AND the same
/// nanosecond, so one retry would almost certainly do; a handful costs
/// nothing and keeps the loop from being a coin flip.
const TEMP_CREATE_ATTEMPTS: u32 = 8;

/// Create a uniquely-named temp file in `dir` with `create_new(true)`, so
/// the open FAILS on collision rather than truncating whatever is already
/// there.
///
/// The name is pid + nanosecond, which is unique in practice but is not a
/// guarantee: two `vc` processes can be issued the same pid on different
/// hosts sharing a filesystem, and a coarse clock can repeat a
/// nanosecond. `File::create` would silently truncate the other writer's
/// half-written temp file and both would then rename over each other —
/// the index is a cache, so the damage is bounded, but "bounded damage"
/// is not the same as "cannot happen". `create_new` turns the collision
/// into an `AlreadyExists` error, which this retries with a fresh suffix;
/// only a genuinely different I/O error propagates.
fn create_temp_file(dir: &Path) -> VcResult<(PathBuf, std::fs::File)> {
    let mut last_err = None;
    for attempt in 0..TEMP_CREATE_ATTEMPTS {
        let tmp = dir.join(format!(
            "index.tmp.{}.{}.{attempt}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(crate::VcError::new(
        crate::ErrorKind::Io,
        format!(
            "index: could not create a temp file in {} after {TEMP_CREATE_ATTEMPTS} attempts{}",
            dir.display(),
            last_err.map(|e| format!(": {e}")).unwrap_or_default()
        ),
    ))
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

    /// Pins the atomicity property from the save() rewrite: after a save,
    /// no `index.tmp.*` sibling is left behind (the temp file was renamed
    /// away, not just written alongside) and the index loads back clean —
    /// i.e. readers can never observe a torn file, only "absent" or
    /// "complete". A true concurrency/torn-write test isn't required; this
    /// pins the write-then-rename mechanism directly.
    #[test]
    fn save_goes_through_rename_leaving_no_tmp_sibling() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "one").unwrap();
        let (_ix, _epoch) = refresh(r).unwrap();

        let tmp_leftovers: Vec<_> = std::fs::read_dir(r.join(".vc"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("index.tmp."))
            })
            .collect();
        assert!(
            tmp_leftovers.is_empty(),
            "save() must leave no temp sibling behind, found: {tmp_leftovers:?}"
        );
        assert!(r.join(".vc/index").is_file(), "final index file exists");

        let loaded = StatIndex::load(r).unwrap().expect("index loads back");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(
            loaded.entries[&PathBuf::from("a.rs")].hash,
            crate::hash::bytes_hash(b"one")
        );
    }

    /// The temp file is opened with `create_new`, so an existing file at
    /// the chosen name is never truncated — the writer picks a different
    /// name instead. Simulated directly by pre-creating every name the
    /// first attempt could pick is impossible (the suffix carries a
    /// nanosecond), so this pins the property that matters: an unrelated
    /// pre-existing `index.tmp.*` file with content of its own survives a
    /// save untouched.
    #[test]
    fn save_never_truncates_an_existing_temp_sibling() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "one").unwrap();

        let squatter = r.join(".vc/index.tmp.99999.1.0");
        std::fs::write(&squatter, b"another writer's half-written temp").unwrap();

        refresh(r).unwrap();

        assert_eq!(
            std::fs::read(&squatter).unwrap(),
            b"another writer's half-written temp",
            "save must never write over a temp file it did not create"
        );
        assert!(r.join(".vc/index").is_file());
    }

    /// `create_temp_file` hands back a distinct path every call, so two
    /// writers in the same process cannot collide on a name.
    #[test]
    fn create_temp_file_returns_distinct_paths() {
        let d = tempfile::tempdir().unwrap();
        let (p1, _f1) = create_temp_file(d.path()).unwrap();
        let (p2, _f2) = create_temp_file(d.path()).unwrap();
        assert_ne!(p1, p2);
        assert!(p1.is_file() && p2.is_file());
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
