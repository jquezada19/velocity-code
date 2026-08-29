use std::path::PathBuf;
use velocity_code_kernel::resolve::EditRequest;
use velocity_code_kernel::{ErrorKind, VcError};

/// Parse one `@@`-header side (`-<start>[,<len>]` or `+<start>[,<len>]`,
/// with the leading `-`/`+` already stripped by the caller) into
/// `(start, len)`. A missing `,<len>` defaults to 1, per unified-diff
/// convention (a header never omits `,1` in practice, but the grammar
/// allows it).
fn parse_start_len(s: &str) -> Option<(usize, usize)> {
    let mut it = s.split(',');
    let start: usize = it.next()?.parse().ok()?;
    let len: usize = match it.next() {
        Some(l) => l.parse().ok()?,
        None => 1,
    };
    Some((start, len))
}

/// Consume exactly the body lines a hunk header promises, by count — never
/// by pattern-matching a body line's own text. A hunk header `@@
/// -old_start,old_len +new_start,new_len @@` promises exactly `old_len`
/// old-side lines (context + deletions) and `new_len` new-side lines
/// (context + additions); inside that counted window every ` `/`-`/`+`
/// prefix is consumed unconditionally, so a deleted line whose own text
/// starts with `--` (e.g. `--x`) or an added line whose own text starts
/// with `++` (e.g. `++i;`) can never be mistaken for a `---`/`+++` file
/// header — there is no prefix special-casing left to fool. Once both
/// counts are satisfied, the window closes immediately (no greedy
/// over-read); running out of input, or a line with any other leading
/// byte, before the counts are satisfied is a zero-fuzz `Malformed`
/// refusal rather than a silently-truncated hunk.
fn consume_counted_hunk_body(
    lines: &mut std::str::Lines<'_>,
    old_len: usize,
    new_len: usize,
    mal: impl Fn(&str) -> VcError,
) -> Result<(Vec<u8>, Vec<u8>), VcError> {
    let (mut old, mut new) = (Vec::new(), Vec::new());
    let (mut old_count, mut new_count) = (0usize, 0usize);
    while old_count < old_len || new_count < new_len {
        let body = lines
            .next()
            .ok_or_else(|| mal("hunk ended before header counts were satisfied"))?;
        match body.as_bytes().first() {
            Some(b' ') => {
                if old_count >= old_len || new_count >= new_len {
                    return Err(mal("hunk has more context lines than its header counts"));
                }
                old.extend_from_slice(&body.as_bytes()[1..]);
                old.push(b'\n');
                new.extend_from_slice(&body.as_bytes()[1..]);
                new.push(b'\n');
                old_count += 1;
                new_count += 1;
            }
            Some(b'-') => {
                if old_count >= old_len {
                    return Err(mal("hunk has more '-' lines than its header's old count"));
                }
                old.extend_from_slice(&body.as_bytes()[1..]);
                old.push(b'\n');
                old_count += 1;
            }
            Some(b'+') => {
                if new_count >= new_len {
                    return Err(mal("hunk has more '+' lines than its header's new count"));
                }
                new.extend_from_slice(&body.as_bytes()[1..]);
                new.push(b'\n');
                new_count += 1;
            }
            Some(b'\\') => {
                return Err(mal(
                    "'\\ No newline' markers unsupported in v0.1 — refused, not fuzzed",
                ));
            }
            _ => return Err(mal("hunk ended before header counts were satisfied")),
        }
    }
    Ok((old, new))
}

pub fn edits_from_diff(diff_text: &str) -> Result<Vec<EditRequest>, VcError> {
    let mut out = Vec::new();
    let mut lines = diff_text.lines();
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
            // "-<old_start>[,<old_len>] +<new_start>[,<new_len>] @@"
            let mut sides = hdr.split_whitespace();
            let old_side = sides
                .next()
                .and_then(|s| s.strip_prefix('-'))
                .ok_or_else(|| mal("bad hunk header"))?;
            let new_side = sides
                .next()
                .and_then(|s| s.strip_prefix('+'))
                .ok_or_else(|| mal("bad hunk header"))?;
            let (old_start, old_len) =
                parse_start_len(old_side).ok_or_else(|| mal("bad hunk header"))?;
            let (_new_start, new_len) =
                parse_start_len(new_side).ok_or_else(|| mal("bad hunk header"))?;

            let (old, new) = consume_counted_hunk_body(&mut lines, old_len, new_len, mal)?;
            if old.is_empty() && new.is_empty() {
                return Err(mal("empty hunk"));
            }
            if old.is_empty() {
                // old_len == 0: a pure insertion / new-file hunk. There is
                // no anchor text for `resolve_edits`' exact-match search
                // to find, so this must refuse honestly here rather than
                // surface as a confusing NotFound/Ambiguous once an empty
                // `old` reaches resolution (an empty needle "matches"
                // everywhere).
                return Err(mal(
                    "insertion-only hunks unsupported in M1 — no anchor text",
                ));
            }
            out.push(EditRequest {
                path,
                old,
                new,
                line_hint: Some(old_start),
            });
        } else if line.starts_with("diff ") || line.starts_with("index ") || line.is_empty() {
            continue; // tolerated git-diff furniture
        } else {
            // Anything here is neither a new file header, a new hunk
            // header, nor tolerated furniture — including a stray line
            // left over after a hunk's counted window has already closed.
            // Silently ignoring it was the other half of the C1 bug (an
            // orphaned line swallowed with no error); zero-fuzz refusal
            // instead.
            return Err(mal(
                "expected a hunk header, a file header, or diff furniture",
            ));
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

    /// C1 regression (a): a hunk that deletes a body line whose own text
    /// starts with `--` (e.g. a Lua/SQL comment `--x`) renders in the diff
    /// as `---x` (the `-` deletion marker plus the literal `--x`) — which
    /// must NOT be mistaken for a `---` file header mid-hunk. Length-count
    /// the hunk from its `@@ -1,2 +1,1 @@` header instead of pattern-
    /// matching the body line's prefix, and the deleted line survives
    /// intact in `old` rather than being silently dropped.
    #[test]
    fn hunk_deleting_a_line_that_starts_with_dashdash_is_not_truncated() {
        let diff = "\
--- a/comment.lua
+++ b/comment.lua
@@ -1,2 +1,1 @@
 keep
---x
";
        let reqs = edits_from_diff(diff).unwrap();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(
            r.old,
            b"keep\n--x\n".to_vec(),
            "deleted '--x' line must survive"
        );
        assert_eq!(r.new, b"keep\n".to_vec());
    }

    /// C1 regression (b): symmetric case on the `+` side — adding a line
    /// whose own text starts with `+` (e.g. C's `++i;`) renders as `+++i;`
    /// and must not be mistaken for a `+++` file header mid-hunk.
    #[test]
    fn hunk_adding_a_line_that_starts_with_plusplus_is_not_truncated() {
        let diff = "\
--- a/loop.c
+++ b/loop.c
@@ -1,1 +1,2 @@
 keep
+++i;
";
        let reqs = edits_from_diff(diff).unwrap();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.old, b"keep\n".to_vec());
        assert_eq!(
            r.new,
            b"keep\n++i;\n".to_vec(),
            "added '++i;' line must survive"
        );
    }

    /// C1 regression (c): the header claims 3 old lines but the body only
    /// supplies 2 before the diff runs out — zero-fuzz refusal, not a
    /// best-effort partial parse.
    #[test]
    fn hunk_header_count_exceeding_body_lines_is_malformed() {
        let diff = "\
--- a/x.txt
+++ b/x.txt
@@ -1,3 +1,3 @@
 a
-b
+B
";
        let e = edits_from_diff(diff).unwrap_err();
        assert!(matches!(e.kind, velocity_code_kernel::ErrorKind::Malformed));
    }

    /// A stray, unrecognized line between hunks (not a new `@@` header, not
    /// a new `---` file header, not tolerated furniture) must refuse
    /// instead of being silently ignored — the general form of the C1 bug.
    #[test]
    fn unrecognized_line_after_a_hunk_closes_is_malformed() {
        let diff = "\
--- a/x.txt
+++ b/x.txt
@@ -1,1 +1,1 @@
-a
+A
this line is not a header or furniture
";
        let e = edits_from_diff(diff).unwrap_err();
        assert!(matches!(e.kind, velocity_code_kernel::ErrorKind::Malformed));
    }

    /// J: a hunk whose old side is empty — `@@ -1,0 +1,N @@`, a pure
    /// insertion with no anchor text on the old side — must be refused
    /// honestly (`Malformed`, naming the reason) rather than falling
    /// through to whatever `resolve_edits` would make of an empty `old`
    /// (an empty needle matches everywhere, which is a `NotFound`/
    /// `Ambiguous` failure far from the real cause).
    #[test]
    fn insertion_only_hunk_is_refused_not_deferred_to_a_later_notfound() {
        let diff = "\
--- a/x.txt
+++ b/x.txt
@@ -1,0 +1,1 @@
+brand new line
";
        let e = edits_from_diff(diff).unwrap_err();
        assert!(matches!(e.kind, velocity_code_kernel::ErrorKind::Malformed));
        assert_eq!(
            e.message,
            "diff: insertion-only hunks unsupported in M1 — no anchor text"
        );
    }

    /// Missing `,len` on a header side defaults to 1, per unified-diff
    /// convention.
    #[test]
    fn header_without_explicit_len_defaults_to_one() {
        let diff = "\
--- a/x.txt
+++ b/x.txt
@@ -1 +1 @@
-a
+A
";
        let reqs = edits_from_diff(diff).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].old, b"a\n".to_vec());
        assert_eq!(reqs[0].new, b"A\n".to_vec());
    }
}
