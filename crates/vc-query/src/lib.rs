//! velocity-code query — literal search with budgets.
//!
//! No write API: this crate only reads files under `root` via
//! [`velocity_code_kernel::walk::walk_scoped`] and never touches the
//! working tree or the `.vc` store.

pub mod render;
pub use render::{Budgeted, render_hits, tokens_est};

use memchr::{memchr_iter, memmem};
use std::fs;
use std::path::{Path, PathBuf};
use velocity_code_kernel::VcResult;
use velocity_code_kernel::walk::walk_scoped;

/// One literal-search match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryHit {
    /// Root-relative path of the matching file.
    pub path: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// 1-based byte column within the line.
    pub col: usize,
    /// The matching line, lossy UTF-8, with no trailing `\n`.
    pub line_text: String,
}

/// Same binary heuristic ripgrep uses by default: a NUL byte anywhere in
/// the first 8KiB marks the file as binary.
const BINARY_SNIFF_LEN: usize = 8192;

/// Literal (non-regex) search for `needle` across the files under `root`
/// named by `scope` (empty = whole tree, per [`walk_scoped`]). Reads each
/// file's bytes once and runs `memmem::find_iter` over the whole buffer,
/// mapping match offsets to line/column via a per-file newline index built
/// with `memchr_iter`. Files with a NUL byte in the first 8KiB are skipped
/// as binary. Results are sorted by `(path, line, col)` and therefore
/// deterministic.
pub fn search_literal(root: &Path, needle: &str, scope: &[PathBuf]) -> VcResult<Vec<QueryHit>> {
    let files = walk_scoped(root, scope)?;
    let needle_bytes = needle.as_bytes();
    let mut hits = Vec::new();

    for rel in files {
        let full = root.join(&rel);
        let bytes = match fs::read(&full) {
            Ok(b) => b,
            // Unreadable (e.g. a broken symlink race) — skip, not fatal.
            Err(_) => continue,
        };
        if is_binary(&bytes) {
            continue;
        }

        let newlines: Vec<usize> = memchr_iter(b'\n', &bytes).collect();
        for pos in memmem::find_iter(&bytes, needle_bytes) {
            let (line, col, line_text) = locate(&bytes, &newlines, pos);
            hits.push(QueryHit {
                path: rel.clone(),
                line,
                col,
                line_text,
            });
        }
    }

    hits.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });
    Ok(hits)
}

fn is_binary(bytes: &[u8]) -> bool {
    let sniff_len = bytes.len().min(BINARY_SNIFF_LEN);
    bytes[..sniff_len].contains(&0u8)
}

/// Maps a byte offset within `bytes` to `(1-based line, 1-based byte col,
/// line text without trailing newline)`, given `newlines` — the sorted
/// byte offsets of every `\n` in `bytes`.
fn locate(bytes: &[u8], newlines: &[usize], pos: usize) -> (usize, usize, String) {
    let line_idx = newlines.partition_point(|&nl| nl < pos);
    let line_start = if line_idx == 0 {
        0
    } else {
        newlines[line_idx - 1] + 1
    };
    let line_end = newlines.get(line_idx).copied().unwrap_or(bytes.len());
    let col = pos - line_start + 1;
    let line_text = String::from_utf8_lossy(&bytes[line_start..line_end]).into_owned();
    (line_idx + 1, col, line_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_search_finds_hits_across_files_in_deterministic_order() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".vc")).unwrap();
        std::fs::write(d.path().join("b.rs"), "let alpha = 1;\nlet beta = alpha;\n").unwrap();
        std::fs::write(d.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        let hits = search_literal(d.path(), "alpha", &[]).unwrap();
        let got: Vec<(String, usize)> = hits
            .iter()
            .map(|h| (h.path.display().to_string(), h.line))
            .collect();
        assert_eq!(
            got,
            vec![("a.rs".into(), 1), ("b.rs".into(), 1), ("b.rs".into(), 2)]
        );
        assert_eq!(hits[0].line_text, "fn alpha() {}");
    }

    #[test]
    fn gitignored_files_are_not_searched() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        std::fs::write(d.path().join(".gitignore"), "gen.rs\n").unwrap();
        std::fs::write(d.path().join("gen.rs"), "let alpha = 1;\n").unwrap();
        std::fs::write(d.path().join("kept.rs"), "let alpha = 2;\n").unwrap();
        let hits = search_literal(d.path(), "alpha", &[]).unwrap();
        let paths: Vec<String> = hits.iter().map(|h| h.path.display().to_string()).collect();
        assert_eq!(paths, vec!["kept.rs".to_string()]);
    }

    #[test]
    fn binary_files_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let mut bytes = b"alpha".to_vec();
        bytes.push(0u8);
        bytes.extend_from_slice(b" more alpha");
        std::fs::write(d.path().join("bin.dat"), &bytes).unwrap();
        let hits = search_literal(d.path(), "alpha", &[]).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn budget_elides_whole_hits_and_reports_count() {
        let d = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for i in 0..100 {
            content.push_str(&format!("hit {i}\n"));
        }
        std::fs::write(d.path().join("many.rs"), content).unwrap();

        let hits = search_literal(d.path(), "hit", &[]).unwrap();
        assert_eq!(hits.len(), 100);

        let three_lines_text = hits[..3]
            .iter()
            .map(|h| format!("{}:{}:{}: {}", h.path.display(), h.line, h.col, h.line_text))
            .collect::<Vec<_>>()
            .join("\n");
        let budget = tokens_est(three_lines_text.len());

        let budgeted = render_hits(&hits, Some(budget));
        assert_eq!(budgeted.text, three_lines_text);
        assert_eq!(budgeted.elided, 97);
    }
}
