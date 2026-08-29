use crate::{ErrorKind, VcError, VcResult, hash, index, resolve, walk};
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

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanForm {
    Edit,
    Import,
    Match,
}

/// The scope + rule a `plan match` was built from: what pattern/rewrite
/// ran, over what scope. Stored on the plan itself (not just re-derivable
/// from CLI args) so `vc show`/replay/audit can see exactly what produced
/// these edits without re-running the selector.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MatchSelector {
    pub pattern: String,
    pub rewrite: String,
    pub lang: String,
    /// Scope as given, root-relative. Empty = whole tree (same convention
    /// as `walk::walk_scoped`).
    pub paths: Vec<PathBuf>,
}

/// Proof of what the selector actually saw: every file in its scope at
/// plan-build time, hashed — not just the files the matched edits touch.
/// This is what lets `plan verify`-style checks tell "nothing in scope
/// changed since the selector ran" apart from "the edited files didn't
/// change" (a file could enter/leave scope, or change without being
/// matched, without the latter noticing).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProvenanceCert {
    /// Write-through index epoch digest when the selector ran.
    pub epoch: String,
    /// Write-through index generation when the selector ran.
    pub generation: u64,
    /// Every in-scope file, root-relative, -> its blake3 hex hash at plan
    /// time.
    pub scope_files: BTreeMap<PathBuf, String>,
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
    /// Match-form only. Absent (and never serialized) on every edit/import
    /// plan — this is what keeps an M1-shaped plan's JSON byte-identical
    /// to before these fields existed, which the stored-plan integrity
    /// check (`Plan::load`) depends on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<MatchSelector>,
    /// Match-form only; same skip-when-absent contract as `selector`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<ProvenanceCert>,
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
        // Single read per file: resolution and hashing both work from the
        // exact bytes read here — no second, independent read later that
        // could observe a file changed in between (see
        // `resolve::resolve_edits_with_content`'s doc comment).
        let (edits, content_by_path) = resolve::resolve_edits_with_content(&root_real, reqs)?;

        // Defense in depth (the CLI's `rebase_user_path` already rebases
        // and root-checks `plan edit`'s file argument): an edit whose path
        // is absolute or escapes via `..` must never reach `files`/
        // `realpaths` — `root_real.join(&edit.path)` would silently ignore
        // `root_real` altogether for an absolute path, and a `..`
        // component can walk back out of it either way. A diff import's
        // internal paths, or a stored plan replayed through `plan
        // refresh`, are the realistic sources of an edit path this
        // untrusted.
        for edit in &edits {
            if !crate::path_is_root_relative(&edit.path) {
                return Err(VcError::new(
                    ErrorKind::Usage,
                    format!(
                        "{}: edit path must be relative with no '..' components",
                        edit.path.display()
                    ),
                ));
            }
        }

        let mut files = BTreeMap::new();
        let mut realpaths = BTreeMap::new();
        for edit in &edits {
            if files.contains_key(&edit.path) {
                continue;
            }
            let abs = root_real.join(&edit.path);
            let content = content_by_path
                .get(&edit.path)
                .expect("resolve_edits_with_content reads every touched file");
            let file_hash = hash::bytes_hash(content);
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
            selector: None,
            certificate: None,
        })
    }

    /// Build a match-form plan from edits already resolved by the caller
    /// (the CLI converts `vc-select`'s `MatchSite`s to `ResolvedEdit`s
    /// before calling this — `vc-kernel` depends on no internal crate, so
    /// `MatchSite` itself must never appear here) plus the exact bytes the
    /// caller read to produce them (`content_by_path` — single-read
    /// discipline, same reasoning as `build`'s use of
    /// `resolve_edits_with_content`: hashing must come from the bytes the
    /// spans were actually computed against, not a second, independent
    /// read).
    ///
    /// Beyond `build`'s `files`/`realpaths`/`edits`, this also builds a
    /// [`ProvenanceCert`]: every file in the selector's scope (walked via
    /// `walk::walk_scoped`), hashed — a named/edited file's hash comes
    /// from `content_by_path`; every other in-scope file is read fresh.
    /// This is strictly wider than `edits`' file set, since scope can
    /// (and typically does) include files the selector looked at but
    /// didn't match.
    pub fn build_match(
        root: &Path,
        selector: MatchSelector,
        edits: Vec<ResolvedEdit>,
        content_by_path: &BTreeMap<PathBuf, Vec<u8>>,
    ) -> VcResult<Plan> {
        let root_real = root
            .canonicalize()
            .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", root.display())))?;

        // Same defense as `build`: an edit path that is absolute or
        // escapes via `..` must never reach `files`/`realpaths`.
        for edit in &edits {
            if !crate::path_is_root_relative(&edit.path) {
                return Err(VcError::new(
                    ErrorKind::Usage,
                    format!(
                        "{}: edit path must be relative with no '..' components",
                        edit.path.display()
                    ),
                ));
            }
        }

        let mut files = BTreeMap::new();
        let mut realpaths = BTreeMap::new();
        for edit in &edits {
            if files.contains_key(&edit.path) {
                continue;
            }
            let abs = root_real.join(&edit.path);
            let content = content_by_path.get(&edit.path).expect(
                "caller must supply content_by_path bytes for every edit's path (single-read discipline)",
            );
            let file_hash = hash::bytes_hash(content);
            let real = abs
                .canonicalize()
                .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", abs.display())))?;
            files.insert(edit.path.clone(), file_hash);
            realpaths.insert(edit.path.clone(), real);
        }

        let (ix, epoch) = index::refresh(&root_real)?;
        let generation = ix.generation;

        // The certificate covers the selector's whole scope, not just the
        // files matched edits touch: a named file's hash reuses the bytes
        // already in `content_by_path` (no second read); every other
        // in-scope file is read fresh for its cert hash.
        let scoped = walk::walk_scoped(&root_real, &selector.paths)?;
        let mut scope_files = BTreeMap::new();
        for rel in scoped {
            let file_hash = match content_by_path.get(&rel) {
                Some(content) => hash::bytes_hash(content),
                None => hash::file_hash(&root_real.join(&rel))?,
            };
            scope_files.insert(rel, file_hash);
        }
        let certificate = ProvenanceCert {
            epoch: epoch.clone(),
            generation,
            scope_files,
        };

        let expected_count = edits.len();
        let created_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Ok(Plan {
            version: 2,
            form: PlanForm::Match,
            root_real,
            epoch,
            files,
            realpaths,
            edits,
            expected_count,
            created_unix,
            selector: Some(selector),
            certificate: Some(certificate),
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
                let plan: Plan = serde_json::from_slice(&bytes).map_err(|e| {
                    VcError::new(ErrorKind::Malformed, format!("plan {prefix}: {e}"))
                })?;
                // Content-addressed integrity: the filename IS the plan's
                // own digest of itself (see `store`), so a hand-edited
                // plan file (still-valid JSON, different content) must be
                // caught here rather than trusted just because it parses.
                let filename_id = matches[0]
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if plan.id() != filename_id {
                    return Err(VcError::new(
                        ErrorKind::Malformed,
                        "plan file does not match its id",
                    ));
                }
                Ok(plan)
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
            selector: None,
            certificate: None,
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

    /// A: `load` recomputes the digest of what it just deserialized and
    /// refuses if it no longer matches the filename that named it — a
    /// hand-edited plan file (same address, different content) must be
    /// caught here, not silently trusted just because the JSON parses.
    #[test]
    fn load_refuses_a_plan_file_whose_content_was_tampered_after_storing() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join(".vc")).unwrap();
        let p = sample_plan();
        let sha8 = p.store(root).unwrap();
        let full_id = p.id();
        let path = plans_dir(root).join(format!("{full_id}.json"));

        let mut on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        on_disk["edits"][0]["new_b64"] = serde_json::json!(base64e(b"tampered"));
        std::fs::write(&path, serde_json::to_vec_pretty(&on_disk).unwrap()).unwrap();

        let err = Plan::load(root, &sha8).unwrap_err();
        assert!(matches!(err.kind, crate::ErrorKind::Malformed));
        assert_eq!(err.message, "plan file does not match its id");
    }

    /// Id-stability regression (Task 12, Step 1): every M1-stored plan on
    /// disk must keep loading and keep passing the integrity check once
    /// `selector`/`certificate` exist on `Plan` — which only holds if both
    /// new fields are `#[serde(default, skip_serializing_if =
    /// "Option::is_none")]`. A `sample_plan()` with both `None` must
    /// serialize with no trace of either key.
    #[test]
    fn edit_form_plan_serialization_is_byte_identical_to_m1_shape() {
        let json = serde_json::to_string(&sample_plan()).unwrap();
        assert!(!json.contains("selector"), "None fields must not serialize");
        assert!(!json.contains("certificate"));
    }

    /// The stronger half of the regression: a literal M1-era plan JSON
    /// (captured before `selector`/`certificate` ever existed — no such
    /// keys anywhere) must still deserialize, and `Plan::load`'s
    /// content-addressed integrity check (recomputed id == filename) must
    /// still pass. If the new fields ever stop skip-serializing when
    /// `None`, the recomputed id for this same JSON changes and every
    /// plan a user has stored under M1 becomes unloadable.
    #[test]
    fn m1_stored_plan_json_still_loads_and_passes_integrity() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir_all(root.join(".vc")).unwrap();

        let m1_json = format!(
            r#"{{"version":1,"form":"Edit","root_real":"/tmp/r","epoch":"{e}","files":{{"a.rs":"{h}"}},"realpaths":{{"a.rs":"/tmp/r/a.rs"}},"edits":[{{"path":"a.rs","start":0,"end":3,"old_b64":"{o}","new_b64":"{n}"}}],"expected_count":1,"created_unix":1756400000}}"#,
            e = "e".repeat(64),
            h = "h".repeat(64),
            o = base64e(b"one"),
            n = base64e(b"two"),
        );
        assert!(!m1_json.contains("selector"));
        assert!(!m1_json.contains("certificate"));

        let parsed: Plan = serde_json::from_str(&m1_json).expect("M1 JSON must still deserialize");
        let id = parsed.id();
        std::fs::create_dir_all(plans_dir(root)).unwrap();
        std::fs::write(plans_dir(root).join(format!("{id}.json")), &m1_json).unwrap();

        let loaded = Plan::load(root, &id).expect("M1 plan must load and pass integrity check");
        assert_eq!(loaded.id(), id);
        assert!(loaded.selector.is_none());
        assert!(loaded.certificate.is_none());
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

    /// B1: an edit request whose resolved path is absolute must be refused
    /// (`Usage`) rather than stored — `root_real.join(&edit.path)` would
    /// silently ignore `root_real` entirely for an absolute path
    /// (`Path::join` replaces, not appends), so `Plan::build` must reject
    /// it before ever joining. Uses a genuinely separate tempdir (not a
    /// real system file) so the test is portable and never touches a real
    /// file's content.
    #[test]
    fn build_refuses_an_edit_path_that_is_absolute() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("repo");
        std::fs::create_dir_all(r.join(".vc")).unwrap();

        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "one\n").unwrap();

        let reqs = vec![resolve::EditRequest {
            path: outside_file.clone(),
            old: b"one".to_vec(),
            new: b"ONE".to_vec(),
            line_hint: None,
        }];
        let err = Plan::build(&r, PlanForm::Edit, &reqs).unwrap_err();
        assert!(matches!(err.kind, crate::ErrorKind::Usage));
        assert_eq!(
            std::fs::read_to_string(&outside_file).unwrap(),
            "one\n",
            "file outside root must be untouched by the refusal"
        );
    }

    /// B1, `..` variant: a path reachable only via a parent-dir component
    /// must be refused the same way, even though `resolve_edits` happily
    /// reads it (root-escape checking is `Plan::build`'s job, not
    /// resolve's).
    #[test]
    fn build_refuses_an_edit_path_containing_a_dotdot_component() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("repo");
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        // Sibling of the repo root, reachable only via `..`.
        std::fs::write(d.path().join("escape.txt"), "one\n").unwrap();

        let reqs = vec![resolve::EditRequest {
            path: PathBuf::from("../escape.txt"),
            old: b"one".to_vec(),
            new: b"ONE".to_vec(),
            line_hint: None,
        }];
        let err = Plan::build(&r, PlanForm::Edit, &reqs).unwrap_err();
        assert!(matches!(err.kind, crate::ErrorKind::Usage));
    }

    /// D: `Plan::build`'s recorded hash must equal the hash of the exact
    /// bytes `resolve_edits_with_content` read to compute the edit's
    /// offsets — not a second, independent read. There is no test hook to
    /// interleave a mutation between the two (there is no "two" any
    /// more — that's the point), so this pins the observable contract the
    /// task describes: build's hash matches a hash of bytes read at
    /// essentially the same moment resolution ran, from the one buffer
    /// resolution actually saw.
    #[test]
    fn build_hash_reflects_the_single_read_resolve_saw() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        let content = b"fn one() { let v = 1; }\n";
        std::fs::write(r.join("a.rs"), content).unwrap();
        let pre_read = std::fs::read(r.join("a.rs")).unwrap();

        let reqs = vec![resolve::EditRequest {
            path: "a.rs".into(),
            old: b"one".to_vec(),
            new: b"uno".to_vec(),
            line_hint: None,
        }];
        let p = Plan::build(&r, PlanForm::Edit, &reqs).unwrap();

        assert_eq!(
            p.files.get(&PathBuf::from("a.rs")),
            Some(&crate::hash::bytes_hash(&pre_read)),
            "build's recorded hash must equal bytes_hash of the content resolve saw"
        );
    }

    fn sample_selector(paths: Vec<PathBuf>) -> MatchSelector {
        MatchSelector {
            pattern: "foo($$$ARGS)".to_string(),
            rewrite: "bar($$$ARGS)".to_string(),
            lang: "rust".to_string(),
            paths,
        }
    }

    /// build_match populates files/realpaths/edits from the given
    /// `ResolvedEdit`s exactly like `build` does, but the certificate must
    /// cover the whole selector scope — including `b.rs`, which is
    /// in-scope but untouched by any edit.
    #[test]
    fn build_match_populates_files_realpaths_edits_and_a_wider_certificate() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(r.join("a.rs"), "fn one() {}\n").unwrap();
        std::fs::write(r.join("b.rs"), "fn two() {}\n").unwrap();

        let edit = ResolvedEdit {
            path: PathBuf::from("a.rs"),
            start: 3,
            end: 6,
            old_b64: base64e(b"one"),
            new_b64: base64e(b"uno"),
        };
        let content_by_path: BTreeMap<PathBuf, Vec<u8>> =
            [(PathBuf::from("a.rs"), b"fn one() {}\n".to_vec())]
                .into_iter()
                .collect();
        let selector = sample_selector(vec![]);

        let p =
            Plan::build_match(&r, selector.clone(), vec![edit.clone()], &content_by_path).unwrap();

        assert_eq!(p.version, 2);
        assert_eq!(p.form, PlanForm::Match);
        assert_eq!(p.expected_count, 1);
        assert_eq!(p.edits.len(), 1);
        assert_eq!(p.edits[0].path, edit.path);
        assert_eq!(p.edits[0].start, edit.start);
        assert_eq!(p.edits[0].end, edit.end);
        assert_eq!(p.edits[0].old_b64, edit.old_b64);
        assert_eq!(p.edits[0].new_b64, edit.new_b64);
        assert_eq!(
            p.files.get(&PathBuf::from("a.rs")),
            Some(&crate::hash::bytes_hash(b"fn one() {}\n"))
        );
        assert!(p.realpaths.contains_key(&PathBuf::from("a.rs")));
        assert!(
            !p.files.contains_key(&PathBuf::from("b.rs")),
            "files/realpaths only cover named edits, not the whole scope"
        );

        let cert = p.certificate.expect("match plan must carry a certificate");
        assert_eq!(
            cert.scope_files.get(&PathBuf::from("a.rs")),
            Some(&crate::hash::bytes_hash(b"fn one() {}\n")),
            "named file's cert hash comes from content_by_path"
        );
        assert_eq!(
            cert.scope_files.get(&PathBuf::from("b.rs")),
            Some(&crate::hash::bytes_hash(b"fn two() {}\n")),
            "untouched in-scope file must still be covered, read fresh"
        );
        assert!(!cert.epoch.is_empty());
        assert_eq!(cert.generation, 1, "first refresh on a fresh .vc dir");

        let sel = p.selector.expect("match plan must carry its selector");
        assert_eq!(sel.pattern, selector.pattern);
        assert_eq!(sel.rewrite, selector.rewrite);
        assert_eq!(sel.lang, selector.lang);
    }

    /// The certificate must respect the selector's scope: a file outside
    /// `paths` is neither hashed nor visible in `scope_files`.
    #[test]
    fn build_match_certificate_respects_scoped_paths() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().to_path_buf();
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::create_dir_all(r.join("sub")).unwrap();
        std::fs::write(r.join("sub/a.rs"), "fn one() {}\n").unwrap();
        std::fs::write(r.join("top.rs"), "fn top() {}\n").unwrap();

        let edit = ResolvedEdit {
            path: PathBuf::from("sub/a.rs"),
            start: 3,
            end: 6,
            old_b64: base64e(b"one"),
            new_b64: base64e(b"uno"),
        };
        let content_by_path: BTreeMap<PathBuf, Vec<u8>> =
            [(PathBuf::from("sub/a.rs"), b"fn one() {}\n".to_vec())]
                .into_iter()
                .collect();
        let selector = sample_selector(vec![PathBuf::from("sub")]);

        let p = Plan::build_match(&r, selector, vec![edit], &content_by_path).unwrap();
        let cert = p.certificate.unwrap();
        assert!(cert.scope_files.contains_key(&PathBuf::from("sub/a.rs")));
        assert!(
            !cert.scope_files.contains_key(&PathBuf::from("top.rs")),
            "out-of-scope file must not appear in the certificate"
        );
    }

    /// Same root-escape defense as `build`: an edit path escaping the root
    /// (here via `..`) must be refused, not silently stored.
    #[test]
    fn build_match_refuses_an_edit_path_containing_a_dotdot_component() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path().join("repo");
        std::fs::create_dir_all(r.join(".vc")).unwrap();
        std::fs::write(d.path().join("escape.txt"), "one\n").unwrap();

        let edit = ResolvedEdit {
            path: PathBuf::from("../escape.txt"),
            start: 0,
            end: 3,
            old_b64: base64e(b"one"),
            new_b64: base64e(b"ONE"),
        };
        let content_by_path: BTreeMap<PathBuf, Vec<u8>> =
            [(PathBuf::from("../escape.txt"), b"one\n".to_vec())]
                .into_iter()
                .collect();

        let err = Plan::build_match(&r, sample_selector(vec![]), vec![edit], &content_by_path)
            .unwrap_err();
        assert!(matches!(err.kind, crate::ErrorKind::Usage));
        assert_eq!(
            std::fs::read_to_string(d.path().join("escape.txt")).unwrap(),
            "one\n",
            "file outside root must be untouched by the refusal"
        );
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
