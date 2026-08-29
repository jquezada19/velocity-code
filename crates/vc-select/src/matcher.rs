//! Structural match-and-rewrite over a language grammar, producing
//! byte-exact edit sites.
//!
//! This is the engine behind `vc plan match`. It productionizes the Task 1
//! spike (`tests/astgrep_spike.rs`): the spike proved ast-grep-core can
//! drive our pinned `tree-sitter-rust` grammar; this module turns that into
//! per-match byte spans against the exact buffer read from disk.
//!
//! Three properties matter here and are each pinned by a test:
//!
//! 1. **Byte exactness.** Every site satisfies `&content[start..end] ==
//!    old`, where `content` is the buffer returned in the content map. The
//!    caller hashes that same buffer, so the plan's hash and the plan's
//!    offsets describe one and the same read (single-read discipline,
//!    mirroring `velocity_code_kernel::resolve::resolve_edits_with_content`).
//! 2. **Per match, not per document.** The spike's `Match::replacement` was
//!    the whole rewritten document and its loop was only ever exercised at
//!    N=1. Here each match carries its own span and its own rewrite bytes,
//!    computed against the *original* tree — no reparse between matches, so
//!    offsets never shift underneath us.
//! 3. **Refuse rather than guess.** Overlapping matches, an unparseable
//!    pattern, a rewrite naming a metavariable the pattern never binds, and
//!    an unknown language are all refusals at plan time, not surprises at
//!    apply time.
//!
//! `vc-select` exposes no write API. `match_sites` reads files and returns
//! bytes; only `vc-kernel` writes.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ast_grep_core::matcher::PatternBuilder;
use ast_grep_core::replacer::{Replacer, TemplateFix};
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use ast_grep_core::{AstGrep, Language, Pattern, PatternError};
use velocity_code_kernel::{ErrorKind, VcError, VcResult};

/// Every file that produced at least one [`MatchSite`], keyed by its
/// root-relative path, holding the exact bytes the spans were computed
/// against. Same shape and same purpose as
/// `velocity_code_kernel::resolve::ContentByPath`.
pub type ContentByPath = BTreeMap<PathBuf, Vec<u8>>;

/// One resolved rewrite site: a byte range in one file's original buffer,
/// the bytes currently there, and the bytes that replace them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchSite {
    /// Root-relative path, exactly as it appeared in `scope_files`.
    pub path: PathBuf,
    /// Byte offset of the match in the original buffer.
    pub start: usize,
    /// Exclusive byte offset end of the match in the original buffer.
    pub end: usize,
    /// The matched bytes: always equal to `&content[start..end]`.
    pub old: Vec<u8>,
    /// The rewrite output for *this* match — not for the document.
    pub new: Vec<u8>,
}

/// Minimal `ast-grep-core` [`Language`] over the pinned `tree-sitter-rust`
/// grammar, lifted verbatim from the Task 1 spike.
///
/// ast-grep-core ships built-in language impls only under `#[cfg(test)]`, so
/// every real caller supplies its own. `vc-lang` exports no grammar handle
/// (it constructs `tree_sitter_rust::LANGUAGE` internally and returns
/// `Symbol`s), so this constructs the grammar directly the same way — both
/// crates depend on the one workspace-pinned `tree-sitter-rust`, so there is
/// exactly one grammar version in the build regardless.
///
/// The `expando_char` override is the load-bearing part, and the reason the
/// spike existed: Rust's grammar accepts `$` only inside `macro_rules!`
/// bodies, so a pattern like `fetch_config($$$A)` parses into an ERROR node
/// instead of a call. ast-grep-core's expando mechanism exists for exactly
/// this — substitute `$` for a character the grammar accepts as an
/// identifier lead (`µ`, U+00B5) before parsing the *pattern*. Source text is
/// never pre-processed, so byte offsets into the file are unaffected.
#[derive(Clone)]
struct RustLang;

impl Language for RustLang {
    fn pre_process_pattern<'q>(&self, query: &'q str) -> Cow<'q, str> {
        if query.contains(self.meta_var_char()) {
            Cow::Owned(query.replace(self.meta_var_char(), &self.expando_char().to_string()))
        } else {
            Cow::Borrowed(query)
        }
    }

    fn expando_char(&self) -> char {
        'µ'
    }

    fn kind_to_id(&self, kind: &str) -> u16 {
        self.get_ts_language()
            .id_for_node_kind(kind, /* named */ true)
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.get_ts_language()
            .field_id_for_name(field)
            .map(|f| f.get())
    }

    fn build_pattern(&self, builder: &PatternBuilder) -> Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, self.clone()))
    }
}

impl LanguageExt for RustLang {
    fn get_ts_language(&self) -> TSLanguage {
        tree_sitter_rust::LANGUAGE.into()
    }
}

type RustDoc = StrDoc<RustLang>;

fn usage(msg: impl Into<String>) -> VcError {
    VcError::new(ErrorKind::Usage, msg)
}

/// Build the pattern and validate the rewrite template against it.
///
/// Two refusals beyond ast-grep's own parse errors:
///
/// * `pattern.has_error()` — a pattern tree-sitter could only recover as an
///   ERROR node. ast-grep will happily run it, but an ERROR-rooted pattern
///   reports `potential_kinds() == None`, i.e. "may match any node kind",
///   which is the worst possible input to a rewrite engine. Refuse.
/// * A rewrite metavariable the pattern never binds. ast-grep's template
///   expander substitutes *nothing* for an unbound variable, so `$$$B` in
///   the rewrite of a `$$$A` pattern silently deletes the arguments instead
///   of erroring. Refuse at plan time.
fn build_pattern(pattern: &str, rewrite: &str) -> VcResult<Pattern> {
    let pat = Pattern::try_new(pattern, RustLang)
        .map_err(|e| usage(format!("pattern `{pattern}`: {e}")))?;
    if pat.has_error() {
        return Err(usage(format!(
            "pattern `{pattern}`: does not parse as rust"
        )));
    }
    let defined = pat.defined_vars();
    let template = TemplateFix::try_new(rewrite, &RustLang)
        .map_err(|e| usage(format!("rewrite `{rewrite}`: {e}")))?;
    let mut undefined: Vec<&str> = template
        .used_vars()
        .into_iter()
        .filter(|v| !defined.contains(v))
        .collect();
    if !undefined.is_empty() {
        undefined.sort_unstable();
        return Err(usage(format!(
            "rewrite `{rewrite}`: uses ${} but the pattern never binds it",
            undefined.join(", $")
        ))
        .with_next("bind it in the pattern, or drop it from the rewrite"));
    }
    Ok(pat)
}

/// Collect every rewrite site for `pattern` -> `rewrite` across
/// `scope_files`.
///
/// `scope_files` are root-relative and already language-filtered by the
/// caller; each is read from disk exactly once. Returns
/// `(sites, content_by_path, warnings)`:
///
/// * `sites` — sorted by `(path, start)`.
/// * `content_by_path` — the exact bytes of every file that produced at
///   least one site. Hash these, not a second read: the spans are offsets
///   into precisely these buffers.
/// * `warnings` — one line per file skipped without failing the run (not
///   UTF-8, or its parse tree contains an error). Callers must surface
///   these; a silently skipped file is an incomplete refactor.
///
/// Refusals: `ErrorKind::Usage` for an unknown `lang`, a non-root-relative
/// scope path, an unparseable pattern, or a rewrite naming an unbound
/// metavariable; `ErrorKind::Overlap` when two matches in one file overlap;
/// `NotFound`/`Io` when a scope file cannot be read.
pub fn match_sites(
    root: &Path,
    pattern: &str,
    rewrite: &str,
    lang: &str,
    scope_files: &[PathBuf],
) -> VcResult<(Vec<MatchSite>, ContentByPath, Vec<String>)> {
    if lang != "rust" {
        return Err(usage(format!(
            "unsupported language `{lang}` — vc-select matches rust only"
        )));
    }
    let pat = build_pattern(pattern, rewrite)?;

    let mut sites: Vec<MatchSite> = Vec::new();
    let mut content_by_path: ContentByPath = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    // A repo-wide scope is thousands of paths, so dedupe through a set
    // rather than a linear scan per entry.
    let mut seen: BTreeSet<&Path> = BTreeSet::new();

    for rel in scope_files {
        // Defense in depth against a caller-supplied scope path, matching
        // `Plan::build`'s check: `root.join()` on an absolute path silently
        // discards `root`, and a `..` component walks back out of it.
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(usage(format!(
                "{}: scope path must be relative with no '..' components",
                rel.display()
            )));
        }
        if !seen.insert(rel.as_path()) {
            continue;
        }

        let abs = root.join(rel);
        let bytes = std::fs::read(&abs).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VcError::new(
                ErrorKind::NotFound,
                format!("{}: no such file", rel.display()),
            ),
            _ => VcError::new(ErrorKind::Io, format!("{}: {e}", rel.display())),
        })?;

        let Ok(src) = std::str::from_utf8(&bytes) else {
            warnings.push(format!("{}: skipped — not valid utf-8", rel.display()));
            continue;
        };

        let found = match file_sites(rel, src, &pat, rewrite) {
            Ok(Some(found)) => found,
            Ok(None) => {
                warnings.push(format!(
                    "{}: skipped — source did not parse as rust",
                    rel.display()
                ));
                continue;
            }
            Err(e) => return Err(e),
        };
        if !found.is_empty() {
            content_by_path.insert(rel.clone(), bytes);
            sites.extend(found);
        }
    }

    sites.sort_by(|a, b| a.path.cmp(&b.path).then(a.start.cmp(&b.start)));
    refuse_overlaps(&sites)?;
    Ok((sites, content_by_path, warnings))
}

/// Match one already-read buffer. `Ok(None)` means "this file's parse tree
/// contains an error — skip it".
///
/// The tree is parsed once and never edited, so every `NodeMatch` range is
/// an offset into `src` as given. Per match:
///
/// * **span** — `Replacer::get_replaced_range`, the same range ast-grep's own
///   `Root::replace` would delete. It is the node's range narrowed to what
///   the pattern actually consumed (`Matcher::get_match_len`), which drops
///   trailing punctuation the pattern didn't ask for: matching `foo($A)`
///   against `foo(1);` yields `foo(1)`, not `foo(1);`.
/// * **rewrite** — `Replacer::generate_replacement`, the template with this
///   match's own metavariable bindings substituted from its own
///   `MetaVarEnv`. A `$$$A` binding expands to the source text spanning its
///   first captured node's start through its last captured node's end, so
///   `fetch_config(a, b)` -> `load_config(a, b)` keeps the separators
///   verbatim.
fn file_sites(
    rel: &Path,
    src: &str,
    pat: &Pattern,
    rewrite: &str,
) -> VcResult<Option<Vec<MatchSite>>> {
    let doc = StrDoc::try_new(src, RustLang).map_err(|e| {
        VcError::new(
            ErrorKind::Malformed,
            format!("{}: tree-sitter failed to parse: {e}", rel.display()),
        )
    })?;
    let root: AstGrep<RustDoc> = AstGrep::doc(doc);

    // tree-sitter is error-tolerant: its root is `source_file` even for
    // garbage, so "did not parse" has to be read off the tree's error flag
    // rather than the root node's kind. `has_error()` is the underlying
    // tree-sitter node's O(1) flag; a partially recovered tree is refused
    // wholesale because a match resolved against error-recovery nodes is not
    // a span we are willing to write bytes at.
    if root.root().get_inner_node().has_error() {
        return Ok(None);
    }

    let mut out = Vec::new();
    for nm in root.root().find_all(pat) {
        let range = <str as Replacer<RustDoc>>::get_replaced_range(rewrite, &nm, pat);
        let new = <str as Replacer<RustDoc>>::generate_replacement(rewrite, &nm);
        let old = src
            .as_bytes()
            .get(range.clone())
            .ok_or_else(|| {
                VcError::new(
                    ErrorKind::Malformed,
                    format!(
                        "{}: match span {}..{} is outside the {}-byte buffer",
                        rel.display(),
                        range.start,
                        range.end,
                        src.len()
                    ),
                )
            })?
            .to_vec();
        out.push(MatchSite {
            path: rel.to_path_buf(),
            start: range.start,
            end: range.end,
            old,
            new,
        });
    }
    Ok(Some(out))
}

/// Refuse any two matches in one file whose spans overlap — including a
/// nested match inside an outer one, which a depth-first pattern search
/// produces naturally (`fetch_config(fetch_config(a))` matches twice).
///
/// `sites` is already sorted by `(path, start)`, so an overlap can only be
/// between neighbours: with `a.start <= b.start`, they overlap exactly when
/// `b.start < a.end`. Same test and same refusal as the kernel's
/// `resolve_edits_with_content`, run here so the failure lands at plan time,
/// naming both spans, instead of at apply time.
fn refuse_overlaps(sites: &[MatchSite]) -> VcResult<()> {
    for w in sites.windows(2) {
        if w[0].path == w[1].path && w[1].start < w[0].end {
            return Err(VcError::new(
                ErrorKind::Overlap,
                format!(
                    "{}: matches at {}..{} and {}..{} overlap",
                    w[0].path.display(),
                    w[0].start,
                    w[0].end,
                    w[1].start,
                    w[1].end
                ),
            )
            .with_next("narrow the pattern so matches cannot nest"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, content: &str) {
        let p = root.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }

    fn scope(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    /// The load-bearing property: every span indexes the buffer the caller
    /// will hash, and `old` is exactly those bytes. Two files, so path
    /// ordering is exercised too.
    #[test]
    fn spans_are_byte_exact_against_the_returned_content() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "b.rs", "fn b() { fetch_config(x) }\n");
        write(
            d.path(),
            "a.rs",
            "fn main() { fetch_config(a, b); other(); }\n",
        );

        let (sites, content, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["b.rs", "a.rs"]),
        )
        .unwrap();

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(sites.len(), 2);
        // Sorted by (path, start): a.rs before b.rs.
        assert_eq!(sites[0].path, PathBuf::from("a.rs"));
        assert_eq!(sites[1].path, PathBuf::from("b.rs"));

        for s in &sites {
            let buf = content
                .get(&s.path)
                .expect("every site's file is in the content map");
            assert_eq!(
                &buf[s.start..s.end],
                &s.old[..],
                "span {}..{} of {} must equal old",
                s.start,
                s.end,
                s.path.display()
            );
        }
        assert_eq!(sites[0].old, b"fetch_config(a, b)");
        assert_eq!(sites[1].old, b"fetch_config(x)");
    }

    /// `$$$A` must carry a multi-argument list through to the rewrite
    /// verbatim, separators and all — and the rewrite is per match, not the
    /// whole rewritten document (the spike's shape).
    #[test]
    fn multi_arg_metavariable_substitutes_into_the_rewrite() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "a.rs",
            "fn main() { fetch_config(a, b, c); other(); }\n",
        );

        let (sites, _, _) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap();

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].new, b"load_config(a, b, c)");
        // Per match, not per document: the rewrite bytes are the call site
        // alone, with no trace of the untouched `other();`.
        assert!(
            !String::from_utf8(sites[0].new.clone())
                .unwrap()
                .contains("other")
        );
    }

    /// N > 1 in a single file — the case the spike's loop never exercised.
    /// Three distinct, non-overlapping, ascending spans, each independently
    /// byte-exact and each with its own rewrite.
    #[test]
    fn multiple_matches_in_one_file_get_distinct_ascending_spans() {
        let d = tempfile::tempdir().unwrap();
        let src = "fn main() {\n    fetch_config(a);\n    other();\n    fetch_config(b, c);\n    fetch_config();\n}\n";
        write(d.path(), "a.rs", src);

        let (sites, content, _) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap();

        assert_eq!(sites.len(), 3, "sites: {sites:?}");
        let buf = &content[&PathBuf::from("a.rs")];
        assert_eq!(buf, src.as_bytes());
        for w in sites.windows(2) {
            assert!(
                w[0].end <= w[1].start,
                "spans must be ascending and disjoint: {:?} then {:?}",
                w[0],
                w[1]
            );
        }
        for s in &sites {
            assert_eq!(&buf[s.start..s.end], &s.old[..]);
        }
        let news: Vec<String> = sites
            .iter()
            .map(|s| String::from_utf8(s.new.clone()).unwrap())
            .collect();
        assert_eq!(
            news,
            vec!["load_config(a)", "load_config(b, c)", "load_config()"]
        );
        // And the spans really are three different places in the file.
        let starts: Vec<usize> = sites.iter().map(|s| s.start).collect();
        assert_eq!(starts.len(), 3);
        assert!(starts[0] < starts[1] && starts[1] < starts[2]);
    }

    /// A file whose parse tree is broken is skipped with a warning naming
    /// it — never fatal, and never at the cost of the other files' sites.
    #[test]
    fn non_parsing_file_is_skipped_with_a_warning_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "good.rs", "fn main() { fetch_config(a) }\n");
        write(d.path(), "bad.rs", "fn broken( { fetch_config(a) ]]]\n");

        let (sites, content, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["bad.rs", "good.rs"]),
        )
        .unwrap();

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].path, PathBuf::from("good.rs"));
        assert!(!content.contains_key(&PathBuf::from("bad.rs")));
        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("bad.rs"), "warning: {}", warnings[0]);
        assert!(
            warnings[0].contains("did not parse"),
            "warning: {}",
            warnings[0]
        );
    }

    #[test]
    fn non_utf8_file_is_skipped_with_a_warning() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("bin.rs"), [0xff, 0xfe, 0x00]).unwrap();

        let (sites, _, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["bin.rs"]),
        )
        .unwrap();

        assert!(sites.is_empty());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("utf-8"), "warning: {}", warnings[0]);
    }

    /// Nested matches overlap, and a plan that edits both would corrupt the
    /// file. Refuse at plan time, naming the file and both spans.
    #[test]
    fn overlapping_matches_are_refused() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "a.rs",
            "fn main() { fetch_config(fetch_config(a)); }\n",
        );

        let e = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap_err();

        assert_eq!(e.kind, ErrorKind::Overlap);
        assert!(e.message.contains("a.rs"), "message: {}", e.message);
        assert!(e.message.contains("overlap"), "message: {}", e.message);
        // Both spans named.
        assert_eq!(
            e.message.matches("..").count(),
            2,
            "both spans must be named: {}",
            e.message
        );
    }

    #[test]
    fn invalid_pattern_is_usage_carrying_ast_greps_message() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() {}\n");

        // Empty pattern: ast-grep's `NoContent`.
        let e = match_sites(d.path(), "", "x", "rust", &scope(&["a.rs"])).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(
            e.message.contains("No AST root"),
            "ast-grep's own message must survive: {}",
            e.message
        );

        // Bare multi-metavariable root: ast-grep's `RootMultiMetaVar`.
        let e = match_sites(d.path(), "$$$A", "$$$A", "rust", &scope(&["a.rs"])).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(
            e.message.contains("multi meta variable"),
            "ast-grep's own message must survive: {}",
            e.message
        );
    }

    /// An unbound rewrite variable expands to nothing in ast-grep, which
    /// would silently delete the arguments. Refuse instead.
    #[test]
    fn rewrite_naming_an_unbound_metavariable_is_usage() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() { fetch_config(a, b) }\n");

        let e = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$B)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(e.message.contains("$B"), "message: {}", e.message);
    }

    #[test]
    fn unknown_language_is_usage() {
        let d = tempfile::tempdir().unwrap();
        let e = match_sites(d.path(), "a", "b", "python", &scope(&[])).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(e.message.contains("python"), "message: {}", e.message);
    }

    #[test]
    fn escaping_scope_path_is_usage() {
        let d = tempfile::tempdir().unwrap();
        for bad in ["../escape.rs", "sub/../../escape.rs"] {
            let e = match_sites(
                d.path(),
                "fetch_config($$$A)",
                "load_config($$$A)",
                "rust",
                &scope(&[bad]),
            )
            .unwrap_err();
            assert_eq!(e.kind, ErrorKind::Usage, "for {bad}");
        }
        let e = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &[d.path().join("abs.rs")],
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
    }

    #[test]
    fn missing_scope_file_is_not_found() {
        let d = tempfile::tempdir().unwrap();
        let e = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["nope.rs"]),
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
    }

    /// The pattern's span stops where the pattern stops: a trailing `;` the
    /// pattern never asked for stays out of `old`, so applying `new` leaves
    /// it in place.
    #[test]
    fn span_excludes_punctuation_the_pattern_did_not_consume() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() { fetch_config(a); }\n");

        let (sites, content, _) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap();

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].old, b"fetch_config(a)");
        let buf = &content[&PathBuf::from("a.rs")];
        assert_eq!(buf[sites[0].end], b';');
    }

    /// Duplicated scope entries read once and yield one set of sites — a
    /// duplicate must not manufacture a spurious self-overlap.
    #[test]
    fn duplicate_scope_entries_are_deduplicated() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() { fetch_config(a) }\n");

        let (sites, _, _) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs", "a.rs"]),
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
    }

    /// A file with no match contributes neither a site nor a content-map
    /// entry — the map is proportional to the result, not to the scope.
    #[test]
    fn files_without_matches_are_absent_from_the_content_map() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() { other() }\n");

        let (sites, content, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap();
        assert!(sites.is_empty());
        assert!(content.is_empty());
        assert!(warnings.is_empty());
    }

    /// Applying every site's `new` at its span reproduces exactly what a
    /// whole-document ast-grep rewrite would produce. This is the end-to-end
    /// proof that per-match spans + per-match rewrites compose.
    #[test]
    fn applying_all_sites_reproduces_the_whole_document_rewrite() {
        let d = tempfile::tempdir().unwrap();
        let src = "fn main() {\n    fetch_config(a);\n    other();\n    fetch_config(b, c);\n}\n";
        write(d.path(), "a.rs", src);

        let (sites, content, _) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap();

        let buf = &content[&PathBuf::from("a.rs")];
        let mut out: Vec<u8> = Vec::new();
        let mut cursor = 0usize;
        for s in &sites {
            out.extend_from_slice(&buf[cursor..s.start]);
            out.extend_from_slice(&s.new);
            cursor = s.end;
        }
        out.extend_from_slice(&buf[cursor..]);

        let expected =
            "fn main() {\n    load_config(a);\n    other();\n    load_config(b, c);\n}\n";
        assert_eq!(String::from_utf8(out).unwrap(), expected);
    }
}
