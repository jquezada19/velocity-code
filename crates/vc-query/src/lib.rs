//! velocity-code query — literal search with budgets.
//!
//! No write API: this crate only reads files under `root` via
//! [`velocity_code_kernel::walk::walk_scoped`] and never touches the
//! working tree or the `.vc` store.

pub mod render;
pub use render::{Budgeted, render_hits, render_symbol_hits, symbol_kind_label, tokens_est};

use memchr::{memchr_iter, memmem};
use regex::bytes::RegexBuilder;
use std::fs;
use std::path::{Path, PathBuf};
use velocity_code_kernel::index;
use velocity_code_kernel::walk::walk_scoped;
use velocity_code_kernel::{ErrorKind, VcError, VcResult};

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

/// Regex search for `pattern` across the files under `root` named by
/// `scope` (same semantics as [`search_literal`]'s `scope`). Runs
/// `regex::bytes::Regex` over the same whole-file buffers, newline index,
/// and binary-skip heuristic as `search_literal` — one [`QueryHit`] per
/// match, not per line, so a line with two matches yields two hits (same
/// as `search_literal`'s `memmem::find_iter`, which also yields
/// overlap-free non-overlapping matches per occurrence). An invalid
/// pattern is not a panic or an empty result: `RegexBuilder::build`'s own
/// parse error is wrapped as `ErrorKind::Usage`, so the CLI surfaces it as a
/// normal refusal (exit 2) instead of crashing. Results are sorted by
/// `(path, line, col)`, matching `search_literal`.
///
/// Compiled with `.multi_line(true)` (R1 parity ruling, 2026-08-29): rg's
/// default search mode feeds its regex engine one line at a time, so `^`/`$`
/// anchor at every line boundary; without multi-line mode, `Regex` over a
/// whole-file buffer only anchors `^`/`$` at the true start/end of the
/// file, which is a real, silent divergence from rg on any anchored
/// pattern. `\A`/`\z` remain available for whole-buffer anchoring when that
/// is genuinely what's wanted.
pub fn search_regex(root: &Path, pattern: &str, scope: &[PathBuf]) -> VcResult<Vec<QueryHit>> {
    let re = RegexBuilder::new(pattern)
        .multi_line(true)
        .build()
        .map_err(|e| VcError::new(ErrorKind::Usage, e.to_string()))?;
    let files = walk_scoped(root, scope)?;
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
        for m in re.find_iter(&bytes) {
            let (line, col, line_text) = locate(&bytes, &newlines, m.start());
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

/// One symbol-search match: a parsed [`velocity_code_lang::Symbol`] plus the
/// root-relative file it came from.
#[derive(Debug, Clone)]
pub struct SymbolHit {
    /// Root-relative path of the file the symbol was parsed from.
    pub path: PathBuf,
    pub symbol: velocity_code_lang::Symbol,
}

/// Languages `search_symbol` knows how to parse today — checked against
/// each file's *index* `lang` tag (never the extension recomputed
/// on-the-fly), so a `.txt` file is never handed to a language parser no
/// matter what its content looks like. Growing this list to track
/// `vc_lang::symbols`'s coverage (Python arrives in PR B) is a one-line
/// change here.
const SYMBOL_LANGS: &[&str] = &["rust"];

/// Symbol search for `name` across the files under `root` named by `scope`
/// (same semantics as [`search_literal`]'s `scope`).
///
/// Refreshes the stat index first (so it sees any file the caller just
/// touched) and only parses files whose *index* `lang` tag is in
/// [`SYMBOL_LANGS`]. A file whose `vc_lang::symbols` call errors
/// (`ErrorKind::Malformed` — genuinely unparseable, non-blank source) is
/// skipped, not fatal: its root-relative path plus the error message is
/// appended to the returned warnings instead of aborting the whole search.
/// An unreadable or non-UTF8 file is likewise skipped silently, matching
/// `search_literal`/`search_regex`'s existing "unreadable -> skip" policy.
///
/// Two-tier match: an exact-name match (`symbol.name == name`) wins over a
/// case-insensitive substring match of `name` within `symbol.name` (fuzzy
/// is name-only — this crate never fuzzes content search). If any exact
/// matches exist, only those are returned and the `bool` is `false`;
/// otherwise every case-insensitive substring match is returned and the
/// `bool` is `true`. Either tier is sorted by `(path, start_line)`.
///
/// Returns `(hits, fuzzy, warnings)` — a 3-tuple rather than a bare `Vec`
/// or an out-param — because the CLI needs all three from one call: the
/// `fuzzy` flag drives `--json`'s top-level `fuzzy` field and whether each
/// hit gets a `fuzzy_source`, and `warnings` (joined) fills the same
/// `CmdOutcome.warning` slot `apply`/`undo` already use for a non-fatal,
/// surfaced-but-not-failing condition — an out-param would need the same
/// plumbing with more ceremony for no extra clarity at this call site.
pub fn search_symbol(
    root: &Path,
    name: &str,
    scope: &[PathBuf],
) -> VcResult<(Vec<SymbolHit>, bool, Vec<String>)> {
    let (ix, _epoch) = index::refresh(root)?;
    let name_lower = name.to_lowercase();
    let mut exact = Vec::new();
    let mut fuzzy_pool = Vec::new();
    let mut warnings = Vec::new();

    for (rel, entry) in &ix.entries {
        if !SYMBOL_LANGS.contains(&entry.lang.as_str()) {
            continue;
        }
        if !scope.is_empty() && !scope.iter().any(|s| rel.starts_with(s)) {
            continue;
        }
        let full = root.join(rel);
        let src = match fs::read_to_string(&full) {
            Ok(s) => s,
            // Unreadable or non-UTF8 (e.g. a broken symlink race) — skip,
            // not fatal, same policy as search_literal/search_regex.
            Err(_) => continue,
        };
        let syms = match velocity_code_lang::symbols(&src, &entry.lang) {
            Ok(s) => s,
            Err(e) => {
                warnings.push(format!("{}: {}", rel.display(), e.message));
                continue;
            }
        };
        for sym in syms {
            if sym.name == name {
                exact.push(SymbolHit {
                    path: rel.clone(),
                    symbol: sym,
                });
            } else if sym.name.to_lowercase().contains(&name_lower) {
                fuzzy_pool.push(SymbolHit {
                    path: rel.clone(),
                    symbol: sym,
                });
            }
        }
    }

    let sort_key = |h: &SymbolHit| (h.path.clone(), h.symbol.start_line);
    exact.sort_by_key(sort_key);
    fuzzy_pool.sort_by_key(sort_key);

    if !exact.is_empty() {
        Ok((exact, false, warnings))
    } else {
        Ok((fuzzy_pool, true, warnings))
    }
}

/// AST-structural search — the query-mode twin of `vc plan match`'s
/// engine, so `vc query --ast` is literally the dry run of the edit
/// (Task 15). Dispatches to [`velocity_code_select::match_sites`] with an
/// empty rewrite (`""` — the rewrite output is unused for a read-only
/// query, and an empty rewrite is explicitly legal there) and renders
/// each returned [`velocity_code_select::MatchSite`] as a [`QueryHit`] at
/// the match's start byte, through the exact same `locate()` byte-offset
/// -> (line, col, line_text) mapping `search_literal`/`search_regex` use
/// — built from the file's own newline index, not `match_sites`'s span.
///
/// `scope_files` must already be language-filtered by the caller (same
/// contract `match_sites` itself documents) — the CLI does this via the
/// shared lang-inference pipeline `plan match` uses. Returns `(hits,
/// warnings)`: `warnings` is `match_sites`'s own per-file skip
/// diagnostics (not valid UTF-8, or a parse tree containing an error),
/// passed through unchanged for the caller to surface exactly like
/// `plan match`'s `plan.warnings` and `search_symbol`'s `warnings` do.
pub fn search_ast(
    root: &Path,
    pattern: &str,
    lang: &str,
    scope_files: &[PathBuf],
) -> VcResult<(Vec<QueryHit>, Vec<String>)> {
    let (sites, content_by_path, warnings) =
        velocity_code_select::match_sites(root, pattern, "", lang, scope_files)?;

    let mut hits = Vec::with_capacity(sites.len());
    for site in &sites {
        // `match_sites` only omits a file from `content_by_path` when it
        // produced zero sites — every site's own path is guaranteed
        // present, so this is a defensive skip, never expected to fire.
        let Some(bytes) = content_by_path.get(&site.path) else {
            continue;
        };
        let newlines: Vec<usize> = memchr_iter(b'\n', bytes).collect();
        let (line, col, line_text) = locate(bytes, &newlines, site.start);
        hits.push(QueryHit {
            path: site.path.clone(),
            line,
            col,
            line_text,
        });
    }
    // `match_sites` sorts by `(path, start)`; re-sort by `(path, line,
    // col)` to guarantee the same ordering contract `search_literal`/
    // `search_regex` pin, rather than relying on the two orderings
    // happening to coincide.
    hits.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });

    Ok((hits, warnings))
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
    fn regex_search_finds_alternation_across_files_in_deterministic_order() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(d.path().join("b.rs"), "fn beta() {}\n").unwrap();
        let hits = search_regex(d.path(), "fn (alpha|beta)", &[]).unwrap();
        let got: Vec<(String, usize, usize)> = hits
            .iter()
            .map(|h| (h.path.display().to_string(), h.line, h.col))
            .collect();
        assert_eq!(got, vec![("a.rs".into(), 1, 1), ("b.rs".into(), 1, 1)]);
        assert_eq!(hits[0].line_text, "fn alpha() {}");
        assert_eq!(hits[1].line_text, "fn beta() {}");
    }

    #[test]
    fn regex_search_one_hit_per_match_on_a_repeated_pattern() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "aa aa\n").unwrap();
        let hits = search_regex(d.path(), "aa", &[]).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].col, 1);
        assert_eq!(hits[1].col, 4);
    }

    /// R1 parity ruling (2026-08-29): `search_regex` must anchor `^`/`$`
    /// at every line boundary within a file, matching rg's default
    /// line-oriented search mode — not just at the whole-buffer start/end,
    /// which is what a bare (non-multi-line) `Regex` over the whole file
    /// would do. Pins the `.multi_line(true)` behavior directly.
    #[test]
    fn regex_search_anchors_per_line_like_rgs_default_mode() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.rs"),
            "fn alpha() {}\nlet x = 1;\nfn beta() {}\n",
        )
        .unwrap();

        let starts = search_regex(d.path(), "^fn", &[]).unwrap();
        let start_lines: Vec<usize> = starts.iter().map(|h| h.line).collect();
        assert_eq!(
            start_lines,
            vec![1, 3],
            "^ must match at the start of every line, not only the start of the file"
        );

        let ends = search_regex(d.path(), ";$", &[]).unwrap();
        let end_lines: Vec<usize> = ends.iter().map(|h| h.line).collect();
        assert_eq!(
            end_lines,
            vec![2],
            "$ must match at the end of every line, not only the end of the file"
        );
    }

    #[test]
    fn invalid_regex_pattern_is_a_usage_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        let err = search_regex(d.path(), "(", &[]).unwrap_err();
        assert_eq!(err.kind, velocity_code_kernel::ErrorKind::Usage);
        assert!(!err.message.is_empty());
    }

    #[test]
    fn regex_search_skips_binary_files_same_as_literal() {
        let d = tempfile::tempdir().unwrap();
        let mut bytes = b"alpha".to_vec();
        bytes.push(0u8);
        bytes.extend_from_slice(b" more alpha");
        std::fs::write(d.path().join("bin.dat"), &bytes).unwrap();
        let hits = search_regex(d.path(), "alpha", &[]).unwrap();
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

    #[test]
    fn exact_name_match_wins_over_fuzzy_substring_matches() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn id() {}\nfn identify() {}\n").unwrap();
        let (hits, fuzzy, warnings) = search_symbol(d.path(), "id", &[]).unwrap();
        assert!(warnings.is_empty());
        assert!(!fuzzy, "an exact match must not be reported as fuzzy");
        let names: Vec<&str> = hits.iter().map(|h| h.symbol.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["id"],
            "the fuzzy-only 'identify' hit must not leak into an exact-tier result"
        );
    }

    #[test]
    fn fuzzy_substring_match_used_and_flagged_when_no_exact_name_matches() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn identify() {}\nfn other() {}\n").unwrap();
        let (hits, fuzzy, warnings) = search_symbol(d.path(), "id", &[]).unwrap();
        assert!(warnings.is_empty());
        assert!(
            fuzzy,
            "zero exact matches must fall back to fuzzy, flagged true"
        );
        let names: Vec<&str> = hits.iter().map(|h| h.symbol.name.as_str()).collect();
        assert_eq!(names, vec!["identify"]);
    }

    #[test]
    fn malformed_file_is_skipped_with_warning_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("good.rs"), "fn target() {}\n").unwrap();
        std::fs::write(
            d.path().join("bad.rs"),
            "this is not rust code at all, just prose.",
        )
        .unwrap();
        let (hits, fuzzy, warnings) = search_symbol(d.path(), "target", &[]).unwrap();
        assert!(!fuzzy);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].symbol.name, "target");
        assert_eq!(
            warnings.len(),
            1,
            "a malformed file must produce a warning, not fail the whole search"
        );
        assert!(warnings[0].contains("bad.rs"), "got: {:?}", warnings);
    }

    #[test]
    fn unsupported_language_files_are_never_parsed() {
        let d = tempfile::tempdir().unwrap();
        // Content that would itself be flagged Malformed if fed to the
        // rust parser (see the vc-lang unit test using the same string) —
        // asserting zero warnings here proves the .txt file was never
        // even attempted, not merely attempted-and-swallowed.
        std::fs::write(
            d.path().join("notes.txt"),
            "this is not rust code at all, just prose.",
        )
        .unwrap();
        let (hits, _fuzzy, warnings) = search_symbol(d.path(), "prose", &[]).unwrap();
        assert!(hits.is_empty());
        assert!(
            warnings.is_empty(),
            "a non-indexed-as-rust file must not even be attempted: {:?}",
            warnings
        );
    }

    #[test]
    fn ast_search_finds_call_site_and_locates_it_via_newline_index() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.rs"),
            "fn main() {\n    fetch_config(a);\n}\n",
        )
        .unwrap();
        let (hits, warnings) = search_ast(
            d.path(),
            "fetch_config($$$A)",
            "rust",
            &[PathBuf::from("a.rs")],
        )
        .unwrap();
        assert!(warnings.is_empty());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, PathBuf::from("a.rs"));
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].col, 5);
        assert_eq!(hits[0].line_text, "    fetch_config(a);");
    }

    #[test]
    fn ast_search_finds_every_call_site_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.rs"),
            "fn main() { fetch_config(a); fetch_config(b); }\n",
        )
        .unwrap();
        std::fs::write(d.path().join("b.rs"), "fn other() { fetch_config(c); }\n").unwrap();
        let (hits, _warnings) = search_ast(
            d.path(),
            "fetch_config($$$A)",
            "rust",
            &[PathBuf::from("a.rs"), PathBuf::from("b.rs")],
        )
        .unwrap();
        let got: Vec<(String, usize)> = hits
            .iter()
            .map(|h| (h.path.display().to_string(), h.line))
            .collect();
        assert_eq!(
            got,
            vec![("a.rs".into(), 1), ("a.rs".into(), 1), ("b.rs".into(), 1)]
        );
    }

    #[test]
    fn hits_within_a_tier_are_ordered_by_path_then_start_line() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.rs"), "fn dup() {}\n").unwrap();
        std::fs::write(d.path().join("a.rs"), "fn other() {}\nfn dup() {}\n").unwrap();
        let (hits, fuzzy, _) = search_symbol(d.path(), "dup", &[]).unwrap();
        assert!(!fuzzy);
        let got: Vec<(String, usize)> = hits
            .iter()
            .map(|h| (h.path.display().to_string(), h.symbol.start_line))
            .collect();
        assert_eq!(got, vec![("a.rs".into(), 2), ("b.rs".into(), 1)]);
    }
}
