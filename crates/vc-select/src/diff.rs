use std::path::PathBuf;
use velocity_code_kernel::resolve::EditRequest;
use velocity_code_kernel::{ErrorKind, VcError};

pub fn edits_from_diff(diff_text: &str) -> Result<Vec<EditRequest>, VcError> {
    let mut out = Vec::new();
    let mut lines = diff_text.lines().peekable();
    let mut cur_path: Option<PathBuf> = None;
    let mal = |m: &str| VcError::new(ErrorKind::Malformed, format!("diff: {m}"));

    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("--- ") {
            let plus = lines.next().ok_or_else(|| mal("missing +++ line"))?;
            let plus_path = plus
                .strip_prefix("+++ ")
                .ok_or_else(|| mal("missing +++ line"))?;
            let _ = rest; // path authority is the +++ side
            let p = plus_path.trim().trim_start_matches("b/");
            cur_path = Some(PathBuf::from(p));
        } else if let Some(hdr) = line.strip_prefix("@@ ") {
            let path = cur_path
                .clone()
                .ok_or_else(|| mal("hunk before file header"))?;
            // parse "-<start>[,<len>] +<start>[,<len>] @@"
            let old_start: usize = hdr
                .split_whitespace()
                .next()
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| mal("bad hunk header"))?;
            let (mut old, mut new) = (Vec::new(), Vec::new());
            while let Some(&body) = lines.peek() {
                match body.as_bytes().first() {
                    Some(b' ') => {
                        old.extend_from_slice(&body.as_bytes()[1..]);
                        old.push(b'\n');
                        new.extend_from_slice(&body.as_bytes()[1..]);
                        new.push(b'\n');
                        lines.next();
                    }
                    Some(b'-') if !body.starts_with("---") => {
                        old.extend_from_slice(&body.as_bytes()[1..]);
                        old.push(b'\n');
                        lines.next();
                    }
                    Some(b'+') if !body.starts_with("+++") => {
                        new.extend_from_slice(&body.as_bytes()[1..]);
                        new.push(b'\n');
                        lines.next();
                    }
                    Some(b'\\') => {
                        return Err(mal(
                            "'\\ No newline' markers unsupported in v0.1 — refused, not fuzzed",
                        ));
                    }
                    _ => break,
                }
            }
            if old.is_empty() && new.is_empty() {
                return Err(mal("empty hunk"));
            }
            out.push(EditRequest {
                path,
                old,
                new,
                line_hint: Some(old_start),
            });
        } else if line.starts_with("diff ") || line.starts_with("index ") || line.is_empty() {
            continue; // tolerated git-diff furniture
        } else if out.is_empty() && cur_path.is_none() {
            return Err(mal("not a unified diff"));
        }
    }
    if out.is_empty() {
        return Err(mal("no hunks found"));
    }
    Ok(out)
}

pub fn edits_from_args(pairs: &[(PathBuf, String, String)]) -> Vec<EditRequest> {
    pairs
        .iter()
        .map(|(p, old, new)| EditRequest {
            path: p.clone(),
            old: old.clone().into_bytes(),
            new: new.clone().into_bytes(),
            line_hint: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn keep() {}
-fn old_name() {}
+fn new_name() {}
 fn keep2() {}
";

    #[test]
    fn parses_single_hunk_to_edit_request() {
        let reqs = edits_from_diff(DIFF).unwrap();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.path, std::path::PathBuf::from("src/a.rs"));
        assert_eq!(
            r.old,
            b"fn keep() {}\nfn old_name() {}\nfn keep2() {}\n".to_vec()
        );
        assert_eq!(
            r.new,
            b"fn keep() {}\nfn new_name() {}\nfn keep2() {}\n".to_vec()
        );
        assert_eq!(r.line_hint, Some(1));
    }

    #[test]
    fn malformed_header_is_refused() {
        let e = edits_from_diff("not a diff at all\n").unwrap_err();
        assert!(matches!(e.kind, velocity_code_kernel::ErrorKind::Malformed));
    }

    #[test]
    fn two_hunks_two_requests() {
        let diff = "\
--- a/x.txt
+++ b/x.txt
@@ -1,1 +1,1 @@
-a
+A
@@ -10,1 +10,1 @@
-b
+B
";
        let reqs = edits_from_diff(diff).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[1].line_hint, Some(10));
    }
}
