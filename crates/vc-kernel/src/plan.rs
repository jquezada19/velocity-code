use crate::{ErrorKind, VcError, VcResult, hash, index, resolve};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn b64e(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub fn b64d(s: &str) -> VcResult<Vec<u8>> {
    STANDARD
        .decode(s)
        .map_err(|e| VcError::new(ErrorKind::Malformed, format!("base64: {e}")))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ResolvedEdit {
    pub path: PathBuf,
    pub start: usize,
    pub end: usize,
    pub old_b64: String,
    pub new_b64: String,
}

/// Match arrives in M2.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanForm {
    Edit,
    Import,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Plan {
    pub version: u32,
    pub form: PlanForm,
    pub root_real: PathBuf,
    pub epoch: String,
    pub files: BTreeMap<PathBuf, String>,
    pub realpaths: BTreeMap<PathBuf, PathBuf>,
    pub edits: Vec<ResolvedEdit>,
    pub expected_count: usize,
    pub created_unix: u64,
}

fn plans_dir(root: &Path) -> PathBuf {
    root.join(".vc/plans")
}

impl Plan {
    /// Build a plan from edit requests against `root`: canonicalize the
    /// root, resolve every edit (exact-unique match, overlap-checked,
    /// sorted by `(path, start)` — see `resolve::resolve_edits`), snapshot
    /// each touched file's current hash and canonical realpath, and stamp
    /// the current write-through index epoch. `apply::apply_plan`
    /// re-verifies every one of these snapshots against fresh on-disk
    /// state before touching anything, so `build` itself does not need to
    /// be race-free — it's the plan's honest opinion of "now", checked
    /// again at apply time.
    pub fn build(root: &Path, form: PlanForm, reqs: &[resolve::EditRequest]) -> VcResult<Plan> {
        let root_real = root
            .canonicalize()
            .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", root.display())))?;
        let edits = resolve::resolve_edits(&root_real, reqs)?;

        let mut files = BTreeMap::new();
        let mut realpaths = BTreeMap::new();
        for edit in &edits {
            if files.contains_key(&edit.path) {
                continue;
            }
            let abs = root_real.join(&edit.path);
            let file_hash = hash::file_hash(&abs)?;
            let real = abs
                .canonicalize()
                .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", abs.display())))?;
            files.insert(edit.path.clone(), file_hash);
            realpaths.insert(edit.path.clone(), real);
        }

        let (_ix, epoch) = index::refresh(&root_real)?;
        let expected_count = edits.len();
        let created_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Ok(Plan {
            version: 1,
            form,
            root_real,
            epoch,
            files,
            realpaths,
            edits,
            expected_count,
            created_unix,
        })
    }

    /// Canonical plan id: sha256 hex over the canonical JSON form of this
    /// struct. Field order is declaration order (stable); BTreeMaps give
    /// sorted keys; there are no floats anywhere in the struct — so this
    /// digest is deterministic and covers every field.
    pub fn id(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("plan serializes");
        let mut h = Sha256::new();
        h.update(&bytes);
        hex_lower(h.finalize().as_slice())
    }

    pub fn sha8(&self) -> String {
        self.id()[..8].to_string()
    }

    /// Write `.vc/plans/<full-id>.json` (pretty JSON), creating the
    /// directory as needed. Returns the sha8.
    pub fn store(&self, root: &Path) -> VcResult<String> {
        let dir = plans_dir(root);
        std::fs::create_dir_all(&dir)?;
        let id = self.id();
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| VcError::new(ErrorKind::Io, e.to_string()))?;
        std::fs::write(dir.join(format!("{id}.json")), bytes)?;
        Ok(id[..8].to_string())
    }

    /// Unique-prefix lookup over `.vc/plans/*.json`. Zero matches ->
    /// `NotFound` (suggesting `vc status`); more than one -> `Ambiguous`
    /// naming the count; exactly one -> parse (a parse failure is
    /// `Malformed`).
    pub fn load(root: &Path, prefix: &str) -> VcResult<Plan> {
        let dir = plans_dir(root);
        let mut matches: Vec<PathBuf> = Vec::new();
        if dir.is_dir() {
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if name.starts_with(prefix) {
                    matches.push(path);
                }
            }
        }
        match matches.len() {
            0 => Err(
                VcError::new(ErrorKind::NotFound, format!("no plan matches {prefix}"))
                    .with_next("vc status"),
            ),
            1 => {
                let bytes = std::fs::read(&matches[0])?;
                serde_json::from_slice(&bytes)
                    .map_err(|e| VcError::new(ErrorKind::Malformed, format!("plan {prefix}: {e}")))
            }
            n => Err(VcError::new(
                ErrorKind::Ambiguous,
                format!("{n} plans match {prefix}"),
            )),
        }
    }

    /// Diff-ish preview text: per edit, a `--- path @ start..end` header,
    /// then old lines prefixed `-` and new lines prefixed `+`. Bytes stay
    /// authoritative; this is a lossy-UTF-8 display only.
    pub fn preview(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for e in &self.edits {
            let _ = writeln!(out, "--- {} @ {}..{}", e.path.display(), e.start, e.end);
            let old = b64d(&e.old_b64).unwrap_or_default();
            for line in String::from_utf8_lossy(&old).lines() {
                let _ = writeln!(out, "-{line}");
            }
            let new = b64d(&e.new_b64).unwrap_or_default();
            for line in String::from_utf8_lossy(&new).lines() {
                let _ = writeln!(out, "+{line}");
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::b64e as base64e;
    use super::*;

    fn sample_plan() -> Plan {
        Plan {
            version: 1,
            form: PlanForm::Edit,
            root_real: PathBuf::from("/tmp/r"),
            epoch: "e".repeat(64),
            files: [(PathBuf::from("a.rs"), "h".repeat(64))]
                .into_iter()
                .collect(),
            realpaths: [(PathBuf::from("a.rs"), PathBuf::from("/tmp/r/a.rs"))]
                .into_iter()
                .collect(),
            edits: vec![ResolvedEdit {
                path: PathBuf::from("a.rs"),
                start: 0,
                end: 3,
                old_b64: base64e(b"one"),
                new_b64: base64e(b"two"),
            }],
            expected_count: 1,
            created_unix: 1_756_400_000,
        }
    }

    #[test]
    fn digest_is_stable_and_ignores_nothing() {
        let p1 = sample_plan();
        let p2 = sample_plan();
        assert_eq!(p1.id(), p2.id());
        let mut p3 = sample_plan();
        p3.edits[0].new_b64 = base64e(b"three");
        assert_ne!(p1.id(), p3.id());
        assert_eq!(p1.sha8().len(), 8);
    }

    #[test]
    fn store_load_roundtrip_and_prefix_semantics() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join(".vc")).unwrap();
        let p = sample_plan();
        let sha8 = p.store(root).unwrap();
        let loaded = Plan::load(root, &sha8).unwrap();
        assert_eq!(loaded.id(), p.id());
        assert!(matches!(
            Plan::load(root, "zzzzzzzz").unwrap_err().kind,
            crate::ErrorKind::NotFound
        ));
    }

    #[test]
    fn preview_shows_header_and_prefixed_old_new_lines() {
        let p = sample_plan();
        assert_eq!(p.preview(), "--- a.rs @ 0..3\n-one\n+two\n");
    }

    #[test]
    fn build_populates_files_realpaths_epoch_and_expected_count() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "fn one() {}\n").unwrap();

        let reqs = vec![resolve::EditRequest {
            path: "a.rs".into(),
            old: b"one".to_vec(),
            new: b"uno".to_vec(),
            line_hint: None,
        }];
        let p = Plan::build(&r, PlanForm::Edit, &reqs).unwrap();

        assert_eq!(p.expected_count, 1);
        assert_eq!(p.edits.len(), 1);
        assert_eq!(
            p.files.get(&PathBuf::from("a.rs")),
            Some(&crate::hash::bytes_hash(b"fn one() {}\n"))
        );
        assert_eq!(p.root_real, r.canonicalize().unwrap());
        assert!(p.realpaths.contains_key(&PathBuf::from("a.rs")));
        assert!(!p.epoch.is_empty());
    }

    #[test]
    fn build_dedupes_a_file_touched_by_multiple_edits() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "one two\n").unwrap();

        let reqs = vec![
            resolve::EditRequest {
                path: "a.rs".into(),
                old: b"one".to_vec(),
                new: b"ONE".to_vec(),
                line_hint: None,
            },
            resolve::EditRequest {
                path: "a.rs".into(),
                old: b"two".to_vec(),
                new: b"TWO".to_vec(),
                line_hint: None,
            },
        ];
        let p = Plan::build(&r, PlanForm::Edit, &reqs).unwrap();

        assert_eq!(p.edits.len(), 2);
        assert_eq!(p.files.len(), 1, "one entry per touched file, not per edit");
    }

    #[test]
    fn build_propagates_resolve_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "one\n").unwrap();

        let reqs = vec![resolve::EditRequest {
            path: "a.rs".into(),
            old: b"missing".to_vec(),
            new: b"x".to_vec(),
            line_hint: None,
        }];
        let err = Plan::build(&r, PlanForm::Edit, &reqs).unwrap_err();
        assert!(matches!(err.kind, crate::ErrorKind::NotFound));
    }
}
