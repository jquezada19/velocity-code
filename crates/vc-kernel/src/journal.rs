use crate::{ErrorKind, VcError, VcResult};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileImage {
    pub path: PathBuf,
    pub pre_b64: String,
    pub pre_hash: String,
    pub post_hash: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JournalEntry {
    pub id: String, // "j-000041"
    pub plan_id: String,
    pub created_unix: u64,
    pub files: Vec<FileImage>,
}

pub struct Journal;

fn journal_dir(root: &Path) -> PathBuf {
    root.join(".vc/journal")
}

fn entry_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn marker_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.committed"))
}

impl Journal {
    /// Max numeric suffix of existing `j-*.json` filenames, plus 1,
    /// formatted `j-{:06}`. Missing journal dir counts as empty (-> j-000001).
    pub fn next_id(root: &Path) -> VcResult<String> {
        let dir = journal_dir(root);
        let mut max = 0u64;
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Some(n) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.strip_prefix("j-"))
                    .and_then(|n| n.parse::<u64>().ok())
                {
                    max = max.max(n);
                }
            }
        }
        Ok(format!("j-{:06}", max + 1))
    }

    /// Serialize pretty JSON, write the entry file, fsync the file, then
    /// fsync the journal directory — the file must be durable on disk
    /// *before* apply (Task 9) mutates any user file. Creates the journal
    /// directory if it doesn't exist yet, so apply never has to.
    pub fn write_entry(root: &Path, e: &JournalEntry) -> VcResult<()> {
        let dir = journal_dir(root);
        std::fs::create_dir_all(&dir)?;
        let bytes =
            serde_json::to_vec_pretty(e).map_err(|e| VcError::new(ErrorKind::Io, e.to_string()))?;
        let mut f = std::fs::File::create(entry_path(&dir, &e.id))?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fsync_dir(&dir)?;
        Ok(())
    }

    /// Create the (empty) commit marker, fsync it, then fsync the journal
    /// directory — called only after every file in the entry has landed.
    pub fn mark_committed(root: &Path, id: &str) -> VcResult<()> {
        let dir = journal_dir(root);
        let f = std::fs::File::create(marker_path(&dir, id))?;
        f.sync_all()?;
        fsync_dir(&dir)?;
        Ok(())
    }

    /// Read + parse a journal entry. Missing file -> NotFound; parse
    /// failure -> Malformed.
    pub fn load(root: &Path, id: &str) -> VcResult<JournalEntry> {
        let path = entry_path(&journal_dir(root), id);
        if !path.is_file() {
            return Err(
                VcError::new(ErrorKind::NotFound, format!("no journal entry {id}"))
                    .with_next("vc doctor"),
            );
        }
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| VcError::new(ErrorKind::Malformed, format!("journal {id}: {e}")))
    }

    /// List `j-*.json` ids, partitioned by presence of the matching
    /// `.committed` marker. Both lists sorted ascending.
    pub fn scan(root: &Path) -> VcResult<(Vec<String>, Vec<String>)> {
        let dir = journal_dir(root);
        let mut committed = Vec::new();
        let mut uncommitted = Vec::new();
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                if !id.starts_with("j-") {
                    continue; // match "j-*.json" literally, not just "*.json"
                }
                if marker_path(&dir, &id).is_file() {
                    committed.push(id);
                } else {
                    uncommitted.push(id);
                }
            }
        }
        committed.sort();
        uncommitted.sort();
        Ok((committed, uncommitted))
    }

    /// Highest committed id, or None if no entry is committed yet.
    pub fn last_committed(root: &Path) -> VcResult<Option<String>> {
        let (mut committed, _) = Self::scan(root)?;
        Ok(committed.pop())
    }
}

/// Open `dir` and fsync it — durably persists directory-entry changes
/// (create/rename/unlink) made within it. Unix only.
pub fn fsync_dir(dir: &Path) -> VcResult<()> {
    let f = std::fs::File::open(dir)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc/journal")).unwrap();
        (d, r)
    }

    fn entry(id: &str) -> JournalEntry {
        JournalEntry {
            id: id.into(),
            plan_id: "p".repeat(64),
            created_unix: 1,
            files: vec![FileImage {
                path: "a.rs".into(),
                pre_b64: crate::plan::b64e(b"old"),
                pre_hash: crate::hash::bytes_hash(b"old"),
                post_hash: crate::hash::bytes_hash(b"new"),
            }],
        }
    }

    #[test]
    fn ids_are_sequential() {
        let (_d, r) = setup();
        assert_eq!(Journal::next_id(&r).unwrap(), "j-000001");
        Journal::write_entry(&r, &entry("j-000001")).unwrap();
        assert_eq!(Journal::next_id(&r).unwrap(), "j-000002");
    }

    #[test]
    fn scan_separates_committed_from_uncommitted() {
        let (_d, r) = setup();
        Journal::write_entry(&r, &entry("j-000001")).unwrap();
        Journal::mark_committed(&r, "j-000001").unwrap();
        Journal::write_entry(&r, &entry("j-000002")).unwrap();
        let (c, u) = Journal::scan(&r).unwrap();
        assert_eq!(c, vec!["j-000001"]);
        assert_eq!(u, vec!["j-000002"]);
        assert_eq!(
            Journal::last_committed(&r).unwrap(),
            Some("j-000001".into())
        );
        let e = Journal::load(&r, "j-000001").unwrap();
        assert_eq!(e.files[0].path, PathBuf::from("a.rs"));
    }

    /// Own test #1: load() of an id with no entry file -> NotFound (not a
    /// generic Io error), whether or not the journal dir even exists.
    #[test]
    fn load_missing_id_is_not_found() {
        let (_d, r) = setup();
        let err = Journal::load(&r, "j-999999").unwrap_err();
        assert!(matches!(err.kind, ErrorKind::NotFound));
    }

    /// Own test #2: write_entry creates `.vc/journal/` itself when absent —
    /// Task 9's apply must be able to call it on a freshly-initialized repo
    /// without pre-creating the directory.
    #[test]
    fn write_entry_creates_journal_dir_when_absent() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        assert!(!r.join(".vc/journal").exists());
        Journal::write_entry(&r, &entry("j-000001")).unwrap();
        assert!(r.join(".vc/journal").is_dir());
        let e = Journal::load(&r, "j-000001").unwrap();
        assert_eq!(e.id, "j-000001");
    }
}
