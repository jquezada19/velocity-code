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
use std::io::Read as _;
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

/// Largest file any read path here will pull into memory whole — the ONE
/// definition, re-exported from the kernel so `velocity_code_query::
/// MAX_SEARCH_FILE_BYTES` keeps naming it.
///
/// It lives in `vc-kernel` rather than here because the AST matcher
/// (`velocity_code_select::match_sites`) bounds its own reads by the same
/// number, and this crate already depends on `vc-select` — so the shared
/// home has to sit below both. See the kernel's definition for the policy.
pub use velocity_code_kernel::MAX_SEARCH_FILE_BYTES;

/// Longest `line_text` a [`QueryHit`] will carry, in bytes of the source
/// line. A hit past this is rendered as a window CENTERED on the match
/// column, with a `…` marker appended.
///
/// Every hit clones its whole matched line. On a file with one enormous
/// line — minified JS, a single-row CSV, a base64 blob — a short needle
/// therefore costs (matches × line length), which is quadratic in the
/// line and is paid BEFORE `--budget` can trim anything, since the budget
/// only applies at render time. A 4 MB minified bundle with a few
/// thousand hits was measured at 993 MB. Clamping makes each hit's text
/// cost a constant.
///
/// `line`/`col` are NOT affected: they are computed from the file's real
/// newline index and always name the true position in the true line. The
/// R1 lexical parity gate compares `(path, line)` sets only — its jq is
/// `.hits[] | "\(.path):\(.line)"`, and r1_defs reads `.fuzzy` plus
/// `.hits[0].path`/`.line` — so no gate reads `text` and parity is
/// unaffected by this clamp.
const MAX_LINE_TEXT_BYTES: usize = 500;

/// Largest number of hits a single search will materialize. Past this,
/// the search REFUSES (`Usage`) rather than returning a partial answer:
/// the hit vector is built in full before `--budget` sees it, so an
/// unbounded match count is an out-of-memory shape, and silently
/// truncating it would hand back an answer that looks complete and is
/// not. Fail closed — the caller narrows the pattern and asks again.
const MAX_TOTAL_HITS: usize = 100_000;

#[cfg(test)]
thread_local! {
    /// Test-only override for [`MAX_TOTAL_HITS`], so the cap refusal can be
    /// exercised with a handful of matches instead of a hundred thousand.
    /// Thread-local (not a global) because the search runs on the calling
    /// thread and Rust's test harness runs tests in parallel.
    static HIT_CAP_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

fn hit_cap() -> usize {
    #[cfg(test)]
    if let Some(n) = HIT_CAP_OVERRIDE.with(|c| c.get()) {
        return n;
    }
    MAX_TOTAL_HITS
}

/// Refuse once the accumulated hit count passes [`hit_cap`]. Called after
/// every push, so the vector never grows more than one hit past the cap.
fn refuse_if_over_hit_cap(n: usize) -> VcResult<()> {
    let cap = hit_cap();
    if n > cap {
        return Err(
            VcError::new(ErrorKind::Usage, format!("too many hits (>{cap})"))
                .with_next("narrow the pattern, or pass a scope path: vc query <pattern> <paths…>"),
        );
    }
    Ok(())
}

/// Read one candidate for content search, bounded at both ends.
///
/// Returns `None` when the file must be skipped, pushing an explanatory
/// line onto `warnings` for every skip a caller could not otherwise
/// predict (unreadable, over the size cap). A binary file is the one
/// silent skip: it is the documented, expected behaviour of every content
/// search here, exactly as it is for ripgrep, and warning per binary file
/// would bury the real diagnostics.
///
/// Order matters. The size gate runs before any content byte is read;
/// then only the first [`BINARY_SNIFF_LEN`] bytes are read, and a binary
/// file returns without ever pulling in the rest. Previously the whole
/// file was read first and *then* sniffed, so a 40 MiB binary cost a full
/// 40 MiB read to reach a decision that the first 8 KiB already settled.
///
/// **Open once, then bound.** The file is opened first and its size read
/// from that same handle, so the size gate and the content read describe
/// one file rather than two lookups of a path that could have been
/// replaced in between. The size gate is still only an *opinion about the
/// past* — a file can grow after it is stat'd — so the read itself is
/// bounded too: `take(MAX_SEARCH_FILE_BYTES + 1)` over the whole read, and
/// if that extra probe byte materializes the file is treated as over the
/// cap (skipped, with a warning) rather than processed. The returned
/// buffer therefore can never exceed the cap, whatever the file does
/// while it is being read.
fn read_for_search(full: &Path, rel: &Path, warnings: &mut Vec<String>) -> Option<Vec<u8>> {
    let mut file = match fs::File::open(full) {
        Ok(f) => f,
        Err(e) => {
            warnings.push(format!("{}: skipped — {e}", rel.display()));
            return None;
        }
    };
    let md = match file.metadata() {
        Ok(md) => md,
        Err(e) => {
            warnings.push(format!("{}: skipped — {e}", rel.display()));
            return None;
        }
    };
    let len = md.len();
    if len > MAX_SEARCH_FILE_BYTES {
        warnings.push(format!(
            "{}: skipped — {len} bytes exceeds the {MAX_SEARCH_FILE_BYTES}-byte size limit",
            rel.display()
        ));
        return None;
    }

    let mut bytes = Vec::with_capacity(len.min(BINARY_SNIFF_LEN as u64) as usize);
    if let Err(e) = (&mut file)
        .take(BINARY_SNIFF_LEN as u64)
        .read_to_end(&mut bytes)
    {
        warnings.push(format!("{}: skipped — {e}", rel.display()));
        return None;
    }
    if bytes.contains(&0u8) {
        return None;
    }
    // `take` consumed at most the sniff window; the handle is positioned
    // right after it, so this reads the remainder without re-reading. The
    // remainder is bounded so that the TOTAL is at most the cap plus one
    // probe byte — `bytes.len()` here is at most BINARY_SNIFF_LEN, well
    // under the cap, so the subtraction cannot underflow.
    let remaining = MAX_SEARCH_FILE_BYTES + 1 - bytes.len() as u64;
    if let Err(e) = (&mut file).take(remaining).read_to_end(&mut bytes) {
        warnings.push(format!("{}: skipped — {e}", rel.display()));
        return None;
    }
    if bytes.len() as u64 > MAX_SEARCH_FILE_BYTES {
        // The probe byte arrived: the file grew past the cap between the
        // stat above and this read. Same policy as a file that was
        // already too big — skip it, and say so.
        warnings.push(format!(
            "{}: skipped — grew past the {MAX_SEARCH_FILE_BYTES}-byte size limit while being read",
            rel.display()
        ));
        return None;
    }
    Some(bytes)
}

/// Read one symbol-search candidate as source text, bounded at
/// [`MAX_SEARCH_FILE_BYTES`].
///
/// `None` means skipped, with the reason pushed onto `warnings` — there is
/// no silent skip on this path at all (no binary sniff: a file the *index*
/// tagged with a symbol language is a source file by construction, and a
/// NUL in it is a surprise worth a parse warning rather than a silent
/// drop).
///
/// Same **open once, then bound** discipline as [`read_for_search`]: the
/// handle supplies both the size and the bytes, and the read is capped at
/// `MAX + 1` so a file that grows past the cap after being sized is
/// skipped rather than parsed from a truncated buffer. Truncation here
/// would be quietly wrong in a way the caller could not see — a symbol
/// table parsed from most of a file reports real names at real line
/// numbers, with the tail's symbols simply absent.
fn read_capped_source(full: &Path, rel: &Path, warnings: &mut Vec<String>) -> Option<String> {
    let mut file = match fs::File::open(full) {
        Ok(f) => f,
        Err(e) => {
            warnings.push(format!("{}: skipped — {e}", rel.display()));
            return None;
        }
    };
    let len = match file.metadata() {
        Ok(md) => md.len(),
        Err(e) => {
            warnings.push(format!("{}: skipped — {e}", rel.display()));
            return None;
        }
    };
    if len > MAX_SEARCH_FILE_BYTES {
        warnings.push(format!(
            "{}: skipped — {len} bytes exceeds the {MAX_SEARCH_FILE_BYTES}-byte size limit",
            rel.display()
        ));
        return None;
    }

    let mut bytes = Vec::with_capacity(len as usize);
    if let Err(e) = (&mut file)
        .take(MAX_SEARCH_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        warnings.push(format!("{}: skipped — {e}", rel.display()));
        return None;
    }
    if bytes.len() as u64 > MAX_SEARCH_FILE_BYTES {
        warnings.push(format!(
            "{}: skipped — grew past the {MAX_SEARCH_FILE_BYTES}-byte size limit while being read",
            rel.display()
        ));
        return None;
    }
    match String::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(_) => {
            warnings.push(format!("{}: skipped — not valid utf-8", rel.display()));
            None
        }
    }
}

fn sort_hits(hits: &mut [QueryHit]) {
    hits.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.col.cmp(&b.col))
    });
}

/// Literal (non-regex) search for `needle` across the files under `root`
/// named by `scope` (empty = whole tree, per [`walk_scoped`]). Reads each
/// file's bytes once (bounded — see [`read_for_search`]) and runs
/// `memmem::find_iter` over the whole buffer, mapping match offsets to
/// line/column via a per-file newline index built with `memchr_iter`.
/// Files with a NUL byte in the first 8KiB are skipped as binary. Results
/// are sorted by `(path, line, col)` and therefore deterministic.
///
/// Returns `(hits, warnings)`: one warning line per file skipped without
/// failing the search, for the caller to surface — the same contract
/// `search_symbol` and `search_ast` already have, and what makes the
/// "never silently skipped" claim true of every search mode rather than
/// only of the two structural ones.
///
/// An EMPTY needle is refused (`Usage`). `memmem` reports an empty needle
/// as matching at every byte position, so `vc query ""` produced one hit
/// per byte of the tree, all of them materialized before any `--budget`
/// could apply — an out-of-memory shape rather than an answer.
pub fn search_literal(
    root: &Path,
    needle: &str,
    scope: &[PathBuf],
) -> VcResult<(Vec<QueryHit>, Vec<String>)> {
    if needle.is_empty() {
        return Err(VcError::new(
            ErrorKind::Usage,
            "empty pattern — it would match at every byte position in the tree",
        )
        .with_next("vc query <pattern>"));
    }
    let files = walk_scoped(root, scope)?;
    let needle_bytes = needle.as_bytes();
    let mut hits = Vec::new();
    let mut warnings = Vec::new();

    for rel in files {
        let full = root.join(&rel);
        let Some(bytes) = read_for_search(&full, &rel, &mut warnings) else {
            continue;
        };

        let newlines: Vec<usize> = memchr_iter(b'\n', &bytes).collect();
        for pos in memmem::find_iter(&bytes, needle_bytes) {
            let (line, col, line_text) = locate(&bytes, &newlines, pos);
            hits.push(QueryHit {
                path: rel.clone(),
                line,
                col,
                line_text,
            });
            refuse_if_over_hit_cap(hits.len())?;
        }
    }

    sort_hits(&mut hits);
    Ok((hits, warnings))
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
///
/// Returns `(hits, warnings)` on the same contract as [`search_literal`],
/// and refuses (`Usage`) a pattern that MATCHES THE EMPTY STRING — `""`,
/// `a*`, `(foo)?` and friends. Such a pattern has a match at every byte
/// position, so the hit list is the size of the tree and is materialized
/// in full before any `--budget` can trim it. The test is direct rather
/// than syntactic: ask the compiled regex whether it matches `""`.
pub fn search_regex(
    root: &Path,
    pattern: &str,
    scope: &[PathBuf],
) -> VcResult<(Vec<QueryHit>, Vec<String>)> {
    let re = RegexBuilder::new(pattern)
        .multi_line(true)
        .build()
        .map_err(|e| VcError::new(ErrorKind::Usage, e.to_string()))?;
    if re.is_match(b"") {
        return Err(VcError::new(
            ErrorKind::Usage,
            format!(
                "pattern `{pattern}` matches the empty string — it would match at \
                 every byte position in the tree"
            ),
        )
        .with_next("vc query <pattern> --regex"));
    }
    let files = walk_scoped(root, scope)?;
    let mut hits = Vec::new();
    let mut warnings = Vec::new();

    for rel in files {
        let full = root.join(&rel);
        let Some(bytes) = read_for_search(&full, &rel, &mut warnings) else {
            continue;
        };

        let newlines: Vec<usize> = memchr_iter(b'\n', &bytes).collect();
        for m in re.find_iter(&bytes) {
            let (line, col, line_text) = locate(&bytes, &newlines, m.start());
            hits.push(QueryHit {
                path: rel.clone(),
                line,
                col,
                line_text,
            });
            refuse_if_over_hit_cap(hits.len())?;
        }
    }

    sort_hits(&mut hits);
    Ok((hits, warnings))
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
/// An unreadable or non-UTF8 file is skipped the same way — warned, not
/// silently dropped — matching what `search_literal`/`search_regex` now do
/// for their own skips. So is a file over [`MAX_SEARCH_FILE_BYTES`]: the
/// read here is bounded exactly as the content-search read is (see
/// [`read_capped_source`]), so `--symbol` cannot be the one door through
/// which a multi-gigabyte file is materialized whole. A file the search
/// could not look at is a gap in the answer, and the caller has to be able
/// to see it.
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
    // An EMPTY (or whitespace-only) name is refused, for the same reason
    // an empty content pattern is: the exact tier finds nothing, so the
    // search falls through to the fuzzy tier, where `contains("")` is true
    // of every symbol in the tree — every symbol materialized as a hit,
    // before any `--budget` could trim it. That is the same amplification
    // shape `search_literal` already refuses, arriving through the symbol
    // door.
    if name.trim().is_empty() {
        return Err(VcError::new(
            ErrorKind::Usage,
            "empty symbol name — it would match every symbol in the tree",
        )
        .with_next("vc query <name> --symbol"));
    }
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
        // Unreadable, non-UTF8 (e.g. a broken symlink race), or over the
        // size cap — skip, not fatal, but never silently: same policy and
        // the same bound as search_literal/search_regex.
        let Some(src) = read_capped_source(&full, rel, &mut warnings) else {
            continue;
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
/// diagnostics (not valid UTF-8, a parse tree containing an error, or a
/// file over [`MAX_SEARCH_FILE_BYTES`]), passed through unchanged for the
/// caller to surface exactly like `plan match`'s `plan.warnings` and
/// `search_symbol`'s `warnings` do.
///
/// Bounded like every other search mode: past the hit cap this REFUSES
/// (`Usage`) rather than returning a partial answer.
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
        // `match_sites` omits a file from `content_by_path` only when it
        // never read it (over the size cap), and such a file produces no
        // sites — every site's own path is guaranteed present, so this is
        // a defensive skip, never expected to fire.
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
        // The same cap the literal and regex paths enforce, for the same
        // reason: the hit vector is built in full before `--budget` sees
        // it, so an unbounded structural match count is an out-of-memory
        // shape. `--ast` is not exempt just because its hits come from a
        // matcher rather than a scan — a broad pattern over a large tree
        // amplifies exactly as a short needle does. Fail closed.
        refuse_if_over_hit_cap(hits.len())?;
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

/// Maps a byte offset within `bytes` to `(1-based line, 1-based byte col,
/// line text without trailing newline)`, given `newlines` — the sorted
/// byte offsets of every `\n` in `bytes`.
///
/// The returned text is clamped to [`MAX_LINE_TEXT_BYTES`] (see there for
/// why) — a window centered on the match, `…`-marked. `line` and `col`
/// are always the true position in the true, unclamped line.
fn locate(bytes: &[u8], newlines: &[usize], pos: usize) -> (usize, usize, String) {
    let line_idx = newlines.partition_point(|&nl| nl < pos);
    let line_start = if line_idx == 0 {
        0
    } else {
        newlines[line_idx - 1] + 1
    };
    let line_end = newlines.get(line_idx).copied().unwrap_or(bytes.len());
    let col = pos - line_start + 1;
    let (window, clamped) = clamp_line_window(&bytes[line_start..line_end], pos - line_start);
    let mut line_text = String::from_utf8_lossy(window).into_owned();
    if clamped {
        line_text.push('…');
    }
    (line_idx + 1, col, line_text)
}

/// A byte is a UTF-8 sequence boundary unless it is a continuation byte
/// (`10xxxxxx`). Used on raw, possibly-invalid bytes, where
/// `str::is_char_boundary` is unavailable — cutting mid-sequence would
/// turn an intact multi-byte character into replacement characters purely
/// because of where the window happened to land.
fn is_utf8_boundary(b: u8) -> bool {
    (b & 0xC0) != 0x80
}

/// The window of `line` to show for a match at byte offset `match_off`
/// within it, plus whether anything was cut.
///
/// A line at or under [`MAX_LINE_TEXT_BYTES`] is returned whole. Longer,
/// the window is centered on the match (so the match itself is visible
/// even at the far end of a megabyte-long line), pushed back inside the
/// line at either edge, then nudged to UTF-8 sequence boundaries — by at
/// most [`MAX_UTF8_SEQ_BYTES`] per edge, which is what keeps the nudge
/// from eating the window on bytes that are not UTF-8 at all (see
/// [`nudge_start`]).
fn clamp_line_window(line: &[u8], match_off: usize) -> (&[u8], bool) {
    if line.len() <= MAX_LINE_TEXT_BYTES {
        return (line, false);
    }
    let half = MAX_LINE_TEXT_BYTES / 2;
    let mut start = match_off.saturating_sub(half);
    let end = (start + MAX_LINE_TEXT_BYTES).min(line.len());
    // Reclaim the width lost when the window ran off the end of the line.
    start = end.saturating_sub(MAX_LINE_TEXT_BYTES);

    let start = nudge_start(line, start, end);
    let end = nudge_end(line, start, end);
    (&line[start..end], true)
}

/// The longest a single UTF-8 sequence can be. A boundary in genuinely
/// UTF-8 text is therefore always within 3 bytes of any offset, so a scan
/// this long either finds one or proves the bytes are not UTF-8 here.
const MAX_UTF8_SEQ_BYTES: usize = 4;

/// The first UTF-8 sequence boundary at or after `start`, searching at
/// most [`MAX_UTF8_SEQ_BYTES`] and never past `end` — falling back to
/// `start` itself when there is none.
///
/// The fallback is the point. The unbounded walk this replaces would march
/// `start` forward for as long as it kept seeing continuation bytes, which
/// on a long line of non-UTF-8 bytes (latin-1 text, a binary-ish blob that
/// passed the NUL sniff) is the entire window: `start` reaches `end`, the
/// window is empty, and the hit renders as a bare `…` with the match
/// nowhere in it. Keeping the raw edge instead costs at most one
/// replacement character from the lossy conversion — and a lossy edge that
/// SHOWS the match beats a clean window that shows nothing.
fn nudge_start(line: &[u8], start: usize, end: usize) -> usize {
    let mut i = start;
    while i < end && i - start < MAX_UTF8_SEQ_BYTES {
        if is_utf8_boundary(line[i]) {
            return i;
        }
        i += 1;
    }
    start
}

/// The last UTF-8 sequence boundary at or before the exclusive `end`,
/// searching at most [`MAX_UTF8_SEQ_BYTES`] and never below `start` —
/// falling back to `end` itself when there is none, for the same reason
/// [`nudge_start`] falls back.
///
/// `end == line.len()` is always a boundary: there is no next sequence to
/// cut into.
fn nudge_end(line: &[u8], start: usize, end: usize) -> usize {
    let mut i = end;
    while i > start && end - i < MAX_UTF8_SEQ_BYTES {
        if i == line.len() || is_utf8_boundary(line[i]) {
            return i;
        }
        i -= 1;
    }
    end
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
        let (hits, warnings) = search_literal(d.path(), "alpha", &[]).unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
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
        let (hits, warnings) = search_literal(d.path(), "alpha", &[]).unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        let paths: Vec<String> = hits.iter().map(|h| h.path.display().to_string()).collect();
        assert_eq!(paths, vec!["kept.rs".to_string()]);
    }

    /// A binary file is the ONE silent skip, by ruling (2026-08-29) —
    /// rg-equivalent behaviour, and a warning per PNG would bury the
    /// diagnostics that matter. So `warnings.is_empty()` here is the
    /// assertion the design wants, not a gap in the "never silently
    /// skipped" claim: that claim covers unreadable, unparseable and
    /// oversized files, and the README states the binary exception
    /// explicitly.
    #[test]
    fn binary_files_are_skipped() {
        let d = tempfile::tempdir().unwrap();
        let mut bytes = b"alpha".to_vec();
        bytes.push(0u8);
        bytes.extend_from_slice(b" more alpha");
        std::fs::write(d.path().join("bin.dat"), &bytes).unwrap();
        let (hits, warnings) = search_literal(d.path(), "alpha", &[]).unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert!(hits.is_empty());
    }

    #[test]
    fn regex_search_finds_alternation_across_files_in_deterministic_order() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(d.path().join("b.rs"), "fn beta() {}\n").unwrap();
        let (hits, _) = search_regex(d.path(), "fn (alpha|beta)", &[]).unwrap();
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
        let (hits, _) = search_regex(d.path(), "aa", &[]).unwrap();
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

        let (starts, _) = search_regex(d.path(), "^fn", &[]).unwrap();
        let start_lines: Vec<usize> = starts.iter().map(|h| h.line).collect();
        assert_eq!(
            start_lines,
            vec![1, 3],
            "^ must match at the start of every line, not only the start of the file"
        );

        let (ends, _) = search_regex(d.path(), ";$", &[]).unwrap();
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
        let (hits, _) = search_regex(d.path(), "alpha", &[]).unwrap();
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

        let (hits, _) = search_literal(d.path(), "hit", &[]).unwrap();
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

    /// Set the hit cap for the duration of `f` on this thread.
    fn with_hit_cap<T>(cap: usize, f: impl FnOnce() -> T) -> T {
        HIT_CAP_OVERRIDE.with(|c| c.set(Some(cap)));
        let out = f();
        HIT_CAP_OVERRIDE.with(|c| c.set(None));
        out
    }

    /// A single very long line — the minified-bundle shape — must not
    /// make every hit carry a copy of the whole line. The text is clamped
    /// and marked, while `line`/`col` still name the true position.
    #[test]
    fn a_hit_on_a_very_long_line_clamps_its_text_and_marks_it() {
        let d = tempfile::tempdir().unwrap();
        // 2000 bytes of filler, the needle at a known offset, more filler.
        let prefix = "x".repeat(1000);
        let suffix = "y".repeat(1000);
        let line = format!("{prefix}NEEDLE{suffix}");
        assert!(line.len() > MAX_LINE_TEXT_BYTES);
        std::fs::write(d.path().join("min.js"), format!("{line}\n")).unwrap();

        let (hits, _) = search_literal(d.path(), "NEEDLE", &[]).unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];

        // Position is exact, and unaffected by the clamp.
        assert_eq!(h.line, 1);
        assert_eq!(h.col, 1001, "col is the true 1-based byte column");

        // Text is clamped to the window plus the marker, and still shows
        // the match itself.
        assert!(h.line_text.ends_with('…'), "clamped text is marked");
        assert!(
            h.line_text.contains("NEEDLE"),
            "the window is centered on the match, so the match is visible"
        );
        assert!(
            h.line_text.len() <= MAX_LINE_TEXT_BYTES + '…'.len_utf8(),
            "clamped to {MAX_LINE_TEXT_BYTES} bytes + marker, got {}",
            h.line_text.len()
        );

        // The control: a short line is returned whole, unmarked.
        std::fs::write(d.path().join("short.rs"), "let NEEDLE = 1;\n").unwrap();
        let (hits, _) = search_literal(d.path(), "let NEEDLE", &[]).unwrap();
        assert_eq!(hits[0].line_text, "let NEEDLE = 1;");
    }

    /// The clamp must not cut a multi-byte character in half — doing so
    /// would turn intact text into replacement characters purely because
    /// of where the window landed.
    #[test]
    fn the_clamp_window_never_splits_a_multibyte_character() {
        let d = tempfile::tempdir().unwrap();
        // Every filler character is 3 bytes, so a naive byte window would
        // land mid-sequence for most match offsets.
        let filler = "あ".repeat(400);
        std::fs::write(
            d.path().join("wide.txt"),
            format!("{filler}NEEDLE{filler}\n"),
        )
        .unwrap();

        let (hits, _) = search_literal(d.path(), "NEEDLE", &[]).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            !hits[0].line_text.contains('\u{FFFD}'),
            "no replacement characters: {}",
            hits[0].line_text
        );
        assert!(hits[0].line_text.contains("NEEDLE"));
    }

    /// Past the cap the search REFUSES rather than returning a partial
    /// answer — fail closed. Exercised through the test-only cap override
    /// so the fixture stays small; the production cap is 100_000.
    #[test]
    fn a_search_past_the_hit_cap_refuses_instead_of_answering_partially() {
        let d = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for _ in 0..20 {
            content.push_str("tok\n");
        }
        std::fs::write(d.path().join("many.rs"), content).unwrap();

        for regex in [false, true] {
            let err = with_hit_cap(5, || {
                if regex {
                    search_regex(d.path(), "tok", &[]).unwrap_err()
                } else {
                    search_literal(d.path(), "tok", &[]).unwrap_err()
                }
            });
            assert_eq!(
                err.kind,
                velocity_code_kernel::ErrorKind::Usage,
                "regex={regex}"
            );
            assert!(
                err.message.contains("too many hits (>5)"),
                "regex={regex}: {}",
                err.message
            );
            assert!(err.next.is_some(), "regex={regex}: refusal needs a hint");
        }

        // The control: exactly at the cap is still an answer, not a
        // refusal — the refusal fires on EXCEEDING it.
        let (hits, _) = with_hit_cap(20, || search_literal(d.path(), "tok", &[]).unwrap());
        assert_eq!(hits.len(), 20);
    }

    /// An empty literal needle matches at every byte position, so the hit
    /// list would be the size of the tree — materialized in full, since
    /// `--budget` only trims at render time. Refuse instead.
    #[test]
    fn empty_literal_pattern_is_refused_not_amplified() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn alpha() {}\n").unwrap();

        let err = search_literal(d.path(), "", &[]).unwrap_err();
        assert_eq!(err.kind, velocity_code_kernel::ErrorKind::Usage);
        assert!(err.message.contains("empty pattern"), "{}", err.message);

        // The control: one byte of needle is fine.
        let (hits, _) = search_literal(d.path(), "f", &[]).unwrap();
        assert!(!hits.is_empty());
    }

    /// Same amplification through the regex door, which an emptiness check
    /// on the pattern STRING would miss: `a*`, `(x)?` and `^` are all
    /// non-empty patterns that match the empty string, and each yields one
    /// hit per byte. The guard asks the compiled regex directly.
    #[test]
    fn regex_matching_the_empty_string_is_refused_not_amplified() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn alpha() {}\n").unwrap();

        for pattern in ["", "a*", "(alpha)?", "x{0,3}"] {
            let err = search_regex(d.path(), pattern, &[]).unwrap_err();
            assert_eq!(
                err.kind,
                velocity_code_kernel::ErrorKind::Usage,
                "pattern {pattern:?} must be refused"
            );
            assert!(
                err.message.contains("empty string"),
                "pattern {pattern:?}: {}",
                err.message
            );
        }

        // The control: a pattern that cannot match empty still works.
        let (hits, _) = search_regex(d.path(), "a+", &[]).unwrap();
        assert!(!hits.is_empty());
    }

    /// Build a fixture whose only NUL byte sits at exactly `nul_at`, with
    /// a searchable `NEEDLE` on its own line AFTER the sniff window.
    ///
    /// Layout: one long line of `a`, then a line of `b` sized so the NUL
    /// lands where asked, then `NEEDLE here`. Every offset is asserted,
    /// not assumed — the whole point of the fixture is the exact byte.
    fn nul_at_offset_fixture(nul_at: usize) -> Vec<u8> {
        let mut v = vec![b'a'; 8100];
        v.push(b'\n');
        assert!(nul_at > v.len());
        v.extend(std::iter::repeat_n(b'b', nul_at - v.len()));
        v.push(0u8);
        assert_eq!(v.len() - 1, nul_at, "the NUL must sit at byte {nul_at}");
        v.push(b'\n');
        v.extend_from_slice(b"NEEDLE here\n");
        v
    }

    /// The `take(BINARY_SNIFF_LEN)` + `read_to_end` seam, crossed from
    /// both sides. The sniff window is the first 8192 bytes — indices
    /// 0..=8191 — so a NUL at 8191 is INSIDE it and a NUL at 8192 is
    /// not, and the two must land on opposite sides of the binary skip.
    ///
    /// The second case also pins the resume-no-gap property: after the
    /// sniff read, the remainder is read from the same handle at the
    /// position `take` left it, so no byte is skipped and none is read
    /// twice. A gap or an overlap there would move every subsequent
    /// offset, so the `NEEDLE` past the boundary is found at exactly the
    /// right line and column or not at all.
    #[test]
    fn the_binary_sniff_boundary_is_exactly_8192_bytes() {
        let d = tempfile::tempdir().unwrap();

        // Inside the window: binary, skipped silently (rg's own policy).
        let inside = d.path().join("inside.dat");
        std::fs::write(&inside, nul_at_offset_fixture(8191)).unwrap();
        let mut warnings = Vec::new();
        assert!(
            read_for_search(&inside, Path::new("inside.dat"), &mut warnings).is_none(),
            "a NUL at byte 8191 is inside the sniff window"
        );
        assert!(warnings.is_empty(), "binary skips are silent: {warnings:?}");

        // One byte past it: not binary, fully searched.
        let outside = d.path().join("outside.dat");
        let content = nul_at_offset_fixture(8192);
        std::fs::write(&outside, &content).unwrap();
        let mut warnings = Vec::new();
        let read_back = read_for_search(&outside, Path::new("outside.dat"), &mut warnings).unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(
            read_back, content,
            "the remainder resumes exactly where the sniff window ended — \
             no byte skipped, none read twice"
        );

        // ...and the match past the boundary is located exactly.
        let d2 = tempfile::tempdir().unwrap();
        std::fs::write(d2.path().join("outside.dat"), &content).unwrap();
        let (hits, warnings) = search_literal(d2.path(), "NEEDLE", &[]).unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 3, "line 1 is the `a`s, line 2 holds the NUL");
        assert_eq!(hits[0].col, 1);
        assert_eq!(hits[0].line_text, "NEEDLE here");
    }

    /// The bound `read_for_search` promises: whatever it returns is at
    /// most [`MAX_SEARCH_FILE_BYTES`], and a file already past the cap is
    /// skipped outright rather than returned truncated (a truncated buffer
    /// would silently answer a search with part of a file).
    #[test]
    fn read_for_search_never_returns_more_than_the_cap() {
        let d = tempfile::tempdir().unwrap();

        let small = d.path().join("a.rs");
        std::fs::write(&small, "fn alpha() {}\n").unwrap();
        let mut warnings = Vec::new();
        let bytes = read_for_search(&small, Path::new("a.rs"), &mut warnings).unwrap();
        assert!(bytes.len() as u64 <= MAX_SEARCH_FILE_BYTES);
        assert!(warnings.is_empty(), "warnings: {warnings:?}");

        // Sparse (`set_len`), so this costs no real disk.
        let big = d.path().join("big.rs");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_SEARCH_FILE_BYTES + 1).unwrap();
        drop(f);
        let mut warnings = Vec::new();
        assert!(
            read_for_search(&big, Path::new("big.rs"), &mut warnings).is_none(),
            "an over-cap file is skipped, never truncated into an answer"
        );
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    }

    /// A file past the size cap is skipped, and the skip is REPORTED. The
    /// fixture is sparse (`set_len`), so it costs no real disk — the point
    /// is that `metadata` decides before any read, so the body is never
    /// materialized.
    #[test]
    fn oversized_file_is_skipped_with_a_warning() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("small.rs"), "fn alpha() {}\n").unwrap();
        let big = std::fs::File::create(d.path().join("big.rs")).unwrap();
        big.set_len(MAX_SEARCH_FILE_BYTES + 1).unwrap();
        drop(big);

        let (hits, warnings) = search_literal(d.path(), "alpha", &[]).unwrap();
        assert_eq!(hits.len(), 1, "the small file is still searched: {hits:?}");
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("big.rs"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("exceeds"),
            "the warning must say why: {}",
            warnings[0]
        );

        // Regex mode is bounded by the same gate and reports the same way.
        let (_, warnings) = search_regex(d.path(), "alpha", &[]).unwrap();
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("big.rs"), "{}", warnings[0]);
    }

    /// A file the search could not read is a gap in the answer. Both
    /// content modes must say so rather than skipping it silently — the
    /// README's "never silently" claim covers every mode, not only the
    /// structural ones.
    #[cfg(unix)]
    #[test]
    fn unreadable_file_yields_a_warning_not_silence() {
        use std::os::unix::fs::PermissionsExt;

        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("readable.rs"), "fn alpha() {}\n").unwrap();
        let locked = d.path().join("locked.rs");
        std::fs::write(&locked, "fn alpha() {}\n").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let literal = search_literal(d.path(), "alpha", &[]);
        let regex = search_regex(d.path(), "alpha", &[]);

        // Restore before any assertion can panic, so tempdir cleanup is
        // never left at the mercy of a failing test.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

        for (mode, result) in [("literal", literal), ("regex", regex)] {
            let (hits, warnings) = result.unwrap();
            assert_eq!(hits.len(), 1, "{mode}: the readable file still matches");
            assert_eq!(warnings.len(), 1, "{mode} warnings: {warnings:?}");
            assert!(warnings[0].contains("locked.rs"), "{mode}: {}", warnings[0]);
        }
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

    /// The matcher's internal size bound, seen from `--ast`'s side: an
    /// over-cap file in scope is reported through the SAME warnings
    /// channel every other skip uses, and does not fail the query.
    ///
    /// This calls `search_ast` directly with the over-cap file in scope,
    /// which is precisely what the CLI's pre-filter would have removed —
    /// that pre-filter stats the tree and then reopens it, so a file
    /// growing past the cap inside that window arrives here anyway. The
    /// pre-filter buys the friendlier early message; this is the net under
    /// it, and the net is what this test pins.
    #[test]
    fn ast_search_reports_an_over_cap_file_through_the_warnings_channel() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() { fetch_config(a); }\n").unwrap();
        // Sparse, so the fixture costs no real disk.
        let big = std::fs::File::create(d.path().join("big.rs")).unwrap();
        big.set_len(MAX_SEARCH_FILE_BYTES + 1).unwrap();
        drop(big);

        let (hits, warnings) = search_ast(
            d.path(),
            "fetch_config($$$A)",
            "rust",
            &[PathBuf::from("a.rs"), PathBuf::from("big.rs")],
        )
        .unwrap();

        assert_eq!(hits.len(), 1, "the in-cap file is still searched: {hits:?}");
        assert_eq!(hits[0].path, PathBuf::from("a.rs"));
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("big.rs"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("exceeds"),
            "the warning says why: {}",
            warnings[0]
        );
    }

    /// `--symbol` must not be the one door through which an unbounded
    /// whole-file read still happens. An over-cap file is skipped with a
    /// warning, and the rest of the tree is searched normally.
    #[test]
    fn an_over_cap_file_is_skipped_by_symbol_search_with_a_warning() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn target() {}\n").unwrap();
        // Sparse, so the fixture costs no real disk. It is indexed as
        // rust, so `search_symbol` would otherwise read all of it.
        let big = std::fs::File::create(d.path().join("big.rs")).unwrap();
        big.set_len(MAX_SEARCH_FILE_BYTES + 1).unwrap();
        drop(big);

        let (hits, fuzzy, warnings) = search_symbol(d.path(), "target", &[]).unwrap();

        assert!(!fuzzy);
        assert_eq!(hits.len(), 1, "the other file is still searched: {hits:?}");
        assert_eq!(hits[0].path, PathBuf::from("a.rs"));
        assert_eq!(hits[0].symbol.name, "target");
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("big.rs"), "{}", warnings[0]);
        assert!(
            warnings[0].contains("exceeds"),
            "the skip is reported with its reason: {}",
            warnings[0]
        );
    }

    /// The clamp's boundary walk must not eat the window on bytes that are
    /// not UTF-8 at all.
    ///
    /// The fixture is a long line of CONTINUATION bytes (`0x80`), which
    /// contain no UTF-8 sequence boundary anywhere, with an ASCII needle
    /// in the middle. The unbounded walk this pins would push `start`
    /// forward until it found a boundary — reaching the needle — and pull
    /// `end` back until it found one — reaching the needle from the other
    /// side — collapsing the window to nothing: the hit rendered as a bare
    /// `…`, with the match it was centered on nowhere in it.
    ///
    /// Bounded to one sequence's worth of scan, neither edge moves, and
    /// the raw window survives to the lossy conversion. Replacement
    /// characters at the edges are the accepted cost: a lossy window that
    /// SHOWS the match beats a clean window that shows nothing.
    #[test]
    fn the_clamp_window_survives_bytes_that_are_not_utf8_at_all() {
        let d = tempfile::tempdir().unwrap();
        let mut content = vec![0x80u8; 1000];
        content.extend_from_slice(b"NEEDLE");
        content.extend(std::iter::repeat_n(0x80u8, 1000));
        content.push(b'\n');
        // No NUL, so this is not skipped as binary — it is simply not
        // valid UTF-8, which is a real shape (latin-1 source, a minified
        // blob) and not a contrived one.
        std::fs::write(d.path().join("latin.txt"), &content).unwrap();

        let (hits, _) = search_literal(d.path(), "NEEDLE", &[]).unwrap();
        assert_eq!(hits.len(), 1);
        let text = &hits[0].line_text;

        assert!(
            text.contains("NEEDLE"),
            "the match must still be visible in the window: {text:?}"
        );
        assert_ne!(text, "…", "the window must not collapse to just the marker");
        assert_eq!(
            hits[0].col, 1001,
            "col is the true byte column, unaffected by the clamp"
        );
    }

    /// `--ast` is bounded by the same hit cap the literal and regex paths
    /// are. Its hits are built in full before `--budget` can trim them, so
    /// a broad structural pattern over a large tree amplifies exactly as a
    /// short needle does — and a partial answer that looks complete is the
    /// failure this refuses. Exercised through the test-only cap override
    /// so the fixture stays small.
    #[test]
    fn an_ast_search_past_the_hit_cap_refuses_instead_of_answering_partially() {
        let d = tempfile::tempdir().unwrap();
        let mut src = String::from("fn main() {\n");
        for i in 0..20 {
            src.push_str(&format!("    fetch_config(a{i});\n"));
        }
        src.push_str("}\n");
        std::fs::write(d.path().join("many.rs"), src).unwrap();
        let scope = [PathBuf::from("many.rs")];

        let err = with_hit_cap(5, || {
            search_ast(d.path(), "fetch_config($$$A)", "rust", &scope).unwrap_err()
        });
        assert_eq!(err.kind, velocity_code_kernel::ErrorKind::Usage);
        assert!(
            err.message.contains("too many hits (>5)"),
            "{}",
            err.message
        );
        assert!(err.next.is_some(), "a refusal needs a hint");

        // The control: exactly at the cap is still an answer, not a
        // refusal — the refusal fires on EXCEEDING it.
        let (hits, _) = with_hit_cap(20, || {
            search_ast(d.path(), "fetch_config($$$A)", "rust", &scope).unwrap()
        });
        assert_eq!(hits.len(), 20);
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
