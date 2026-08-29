use crate::plan::{ResolvedEdit, b64e};
use crate::{ErrorKind, VcError, VcResult};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Every unique path `resolve_edits_with_content` read, keyed by its
/// root-relative path.
pub type ContentByPath = BTreeMap<PathBuf, Vec<u8>>;

#[derive(Debug)]
pub struct EditRequest {
    pub path: PathBuf,
    pub old: Vec<u8>,
    pub new: Vec<u8>,
    pub line_hint: Option<usize>,
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() {
        return vec![];
    }
    let mut out = vec![];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            out.push(i);
        }
        i += 1;
    }
    out
}

fn line_of(hay: &[u8], byte: usize) -> usize {
    1 + hay[..byte].iter().filter(|&&b| b == b'\n').count()
}

/// Resolve `reqs` and return, alongside the resolved edits, every touched
/// file's content as read to resolve them — each unique path read from
/// disk exactly once, even when multiple requests target the same file.
/// `resolve_edits` is a thin wrapper over this that discards the content
/// map; `Plan::build` uses this directly so its hash comes from the exact
/// bytes resolution saw rather than a second, independent read (a
/// plan-time TOCTOU: without this, a file that changed between resolve's
/// read and a later hashing read could record a hash that doesn't
/// correspond to the offsets computed against the first read).
pub fn resolve_edits_with_content(
    root: &Path,
    reqs: &[EditRequest],
) -> VcResult<(Vec<ResolvedEdit>, ContentByPath)> {
    let mut content_by_path: ContentByPath = BTreeMap::new();
    let mut out: Vec<ResolvedEdit> = Vec::new();
    for req in reqs {
        if !content_by_path.contains_key(&req.path) {
            let bytes = std::fs::read(root.join(&req.path)).map_err(|_| {
                VcError::new(
                    ErrorKind::NotFound,
                    format!("{}: no such file", req.path.display()),
                )
            })?;
            content_by_path.insert(req.path.clone(), bytes);
        }
        let content = &content_by_path[&req.path];
        let hits = find_all(content, &req.old);
        let start = match (hits.len(), req.line_hint) {
            (0, _) => {
                return Err(VcError::new(
                    ErrorKind::NotFound,
                    format!("{}: old text not found", req.path.display()),
                ));
            }
            (1, _) => hits[0],
            (n, Some(hint)) => {
                let at: Vec<usize> = hits
                    .iter()
                    .copied()
                    .filter(|&h| line_of(content, h) == hint)
                    .collect();
                match at.len() {
                    1 => at[0],
                    _ => {
                        return Err(VcError::new(
                            ErrorKind::Ambiguous,
                            format!(
                                "{}: {n} matches, line hint {hint} selects {}",
                                req.path.display(),
                                at.len()
                            ),
                        ));
                    }
                }
            }
            (n, None) => {
                return Err(VcError::new(
                    ErrorKind::Ambiguous,
                    format!(
                        "{}: old text matches {n} times — add more context",
                        req.path.display()
                    ),
                ));
            }
        };
        out.push(ResolvedEdit {
            path: req.path.clone(),
            start,
            end: start + req.old.len(),
            old_b64: b64e(&req.old),
            new_b64: b64e(&req.new),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path).then(a.start.cmp(&b.start)));
    for w in out.windows(2) {
        if w[0].path == w[1].path && w[1].start < w[0].end {
            return Err(VcError::new(
                ErrorKind::Overlap,
                format!(
                    "{}: edits at {}..{} and {}..{} overlap",
                    w[0].path.display(),
                    w[0].start,
                    w[0].end,
                    w[1].start,
                    w[1].end
                ),
            ));
        }
    }
    Ok((out, content_by_path))
}

/// Thin wrapper over [`resolve_edits_with_content`] for callers that only
/// need the resolved edits, not the content read along the way.
pub fn resolve_edits(root: &Path, reqs: &[EditRequest]) -> VcResult<Vec<ResolvedEdit>> {
    resolve_edits_with_content(root, reqs).map(|(edits, _)| edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, content: &str) {
        std::fs::write(root.join(name), content).unwrap();
    }

    #[test]
    fn unique_match_resolves() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn one() {}\nfn two() {}\n");
        let r = resolve_edits(
            d.path(),
            &[EditRequest {
                path: "a.rs".into(),
                old: b"fn one()".to_vec(),
                new: b"fn uno()".to_vec(),
                line_hint: None,
            }],
        )
        .unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].start, r[0].end), (0, 8));
    }

    #[test]
    fn zero_matches_is_not_found_and_two_is_ambiguous() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "x\nx\n");
        let nf = resolve_edits(
            d.path(),
            &[EditRequest {
                path: "a.rs".into(),
                old: b"y".to_vec(),
                new: b"z".to_vec(),
                line_hint: None,
            }],
        )
        .unwrap_err();
        assert!(matches!(nf.kind, crate::ErrorKind::NotFound));
        let amb = resolve_edits(
            d.path(),
            &[EditRequest {
                path: "a.rs".into(),
                old: b"x".to_vec(),
                new: b"z".to_vec(),
                line_hint: None,
            }],
        )
        .unwrap_err();
        assert!(matches!(amb.kind, crate::ErrorKind::Ambiguous));
    }

    #[test]
    fn line_hint_disambiguates() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "x\nx\n");
        let r = resolve_edits(
            d.path(),
            &[EditRequest {
                path: "a.rs".into(),
                old: b"x".to_vec(),
                new: b"z".to_vec(),
                line_hint: Some(2),
            }],
        )
        .unwrap();
        assert_eq!(r[0].start, 2); // second line's x
    }

    #[test]
    fn overlapping_edits_refused() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "abcdef");
        let e = resolve_edits(
            d.path(),
            &[
                EditRequest {
                    path: "a.rs".into(),
                    old: b"abcd".to_vec(),
                    new: b"1".to_vec(),
                    line_hint: None,
                },
                EditRequest {
                    path: "a.rs".into(),
                    old: b"cdef".to_vec(),
                    new: b"2".to_vec(),
                    line_hint: None,
                },
            ],
        )
        .unwrap_err();
        assert!(matches!(e.kind, crate::ErrorKind::Overlap));
    }
}
