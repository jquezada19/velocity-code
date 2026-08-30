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
//!    pattern, a pattern containing the expando character, a rewrite whose
//!    metavariable the pattern does not bind *in the same sigil form*, and an
//!    unknown language are all refusals at plan time, not surprises at apply
//!    time. See [`build_pattern`] for why each of the metavariable refusals
//!    exists: ast-grep substitutes silently rather than erroring, so an
//!    unrefused mismatch deletes arguments instead of failing.
//!
//! `vc-select` exposes no write API. `match_sites` reads files and returns
//! bytes; only `vc-kernel` writes.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use ast_grep_core::matcher::PatternBuilder;
use ast_grep_core::replacer::{Replacer, TemplateFix};
use ast_grep_core::tree_sitter::{LanguageExt, StrDoc, TSLanguage};
use ast_grep_core::{AstGrep, Language, Pattern, PatternError};
use velocity_code_kernel::{ErrorKind, MAX_SEARCH_FILE_BYTES, VcError, VcResult};

/// Every file [`match_sites`] scanned — matched or not — keyed by its
/// root-relative path, holding the exact bytes that scan read. Same shape
/// and same purpose as `velocity_code_kernel::resolve::ContentByPath`.
///
/// The map covers the *whole scanned scope*, not just the files that
/// produced sites, so a caller that must describe the scope it saw (the
/// plan's provenance certificate) can hash these bytes instead of reading
/// the tree a second time. See [`match_sites`] for the memory cost that
/// buys.
///
/// **The one exception is a file over
/// [`velocity_code_kernel::MAX_SEARCH_FILE_BYTES`]**, which is never read
/// and therefore has no entry. "Scanned" means "read"; a file the matcher
/// refused to materialize was not read, and inventing an entry for it —
/// truncated, or from a second read — would put bytes in a certificate
/// that no scan ever saw. A caller that cannot tolerate a gap must refuse
/// on the gap (as `Plan::build_match` does), not paper over it.
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

/// Which sigil a metavariable occurrence was written with. `$NAME` binds one
/// node; `$$$NAME` binds a list of them. ast-grep tracks these as different
/// `MetaVariable` variants and stores them in different halves of the
/// `MetaVarEnv`, but reports both under the bare name from
/// `Pattern::defined_vars` and `TemplateFix::used_vars` — which is exactly
/// why [`build_pattern`] cannot check names alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sigil {
    Single,
    Multi,
}

impl Sigil {
    fn render(self, name: &str) -> String {
        match self {
            Sigil::Single => format!("${name}"),
            Sigil::Multi => format!("$$${name}"),
        }
    }
}

/// The characters ast-grep accepts inside a metavariable name. Mirrors
/// `ast_grep_core::meta_var::is_valid_meta_var_char`, which is crate-private:
/// `A`-`Z`, `_`, `0`-`9`.
fn is_meta_var_char(c: char) -> bool {
    c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit()
}

/// Scan `s` for metavariable occurrences the way ast-grep's own template
/// expander does (`split_first_meta_var`, driven by `create_template`),
/// returning each name with every sigil form it was written with.
///
/// The dollar-run rule is ast-grep's, quirks included: a run of three `$`
/// makes a list capture, a run of one *or two* makes a single capture (`$$A`
/// is `$A`), and a run followed by no valid name is literal text, skipped one
/// byte at a time. Byte indexing is safe because `$` is ASCII and can never
/// appear as a UTF-8 continuation byte, so a name always starts on a char
/// boundary.
fn metavar_forms(s: &str) -> BTreeMap<&str, BTreeSet<Sigil>> {
    let bytes = s.as_bytes();
    let mut out: BTreeMap<&str, BTreeSet<Sigil>> = BTreeMap::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let mut dollars = 1usize;
        while dollars < 3 && bytes.get(i + dollars) == Some(&b'$') {
            dollars += 1;
        }
        let name_start = i + dollars;
        let rest = &s[name_start..];
        let name_len = rest
            .find(|c: char| !is_meta_var_char(c))
            .unwrap_or(rest.len());
        if name_len == 0 {
            i += 1;
            continue;
        }
        let sigil = if dollars == 3 {
            Sigil::Multi
        } else {
            Sigil::Single
        };
        out.entry(&s[name_start..name_start + name_len])
            .or_default()
            .insert(sigil);
        i = name_start + name_len;
    }
    out
}

/// Build the pattern and validate the rewrite template against it.
///
/// Three refusals beyond ast-grep's own parse errors:
///
/// * **The expando character in the pattern.** `pre_process_pattern`
///   rewrites every `$` to `µ` before handing the pattern to tree-sitter, so
///   a `µ` the caller typed themselves is indistinguishable from one we
///   substituted. `µA` is a legal Rust identifier, so the pattern
///   `fetch_config(µA)` — which reads as a literal-identifier match — silently
///   becomes a match-anything capture that would rewrite every call. Refuse
///   any pattern containing `µ`. (The rewrite side is inert: templates are
///   expanded against `meta_var_char()`, i.e. `$`, and never see the expando.)
/// * **`pattern.has_error()`** — a pattern tree-sitter could only recover as
///   an ERROR node. ast-grep will happily run it, but an ERROR-rooted pattern
///   reports `potential_kinds() == None`, i.e. "may match any node kind",
///   which is the worst possible input to a rewrite engine. Refuse.
/// * **A rewrite metavariable the pattern does not bind in the same form.**
///   ast-grep's template expander substitutes *nothing* for a variable that
///   is absent from the env, and `$A` / `$$$A` live in different halves of
///   that env — so `$$$B` in the rewrite of a `$$$A` pattern, and equally
///   `$$$A` in the rewrite of a `$A` pattern, both silently delete the
///   arguments instead of erroring. Both are refused: the name must be bound
///   (`Pattern::defined_vars`) *and* every sigil form the rewrite uses for it
///   must be a form the pattern actually wrote.
///
/// **Residual, deliberately left open.** The sigil half is a string-level
/// scan, so a `$` that tree-sitter absorbs into a terminal — inside a string
/// literal or a comment in the *pattern* — is counted as a binding it isn't
/// (`foo("$A")` scans as binding `$A`). That direction can only make the
/// sigil check *permissive*, never falsely refusing; and the genuinely
/// unbound case it might otherwise wave through is still caught by the
/// `defined_vars` check, which reads the parsed tree rather than the text. A
/// `$` inside a *rewrite* string literal needs no special handling: ast-grep's
/// own expander is a string scan too, so it substitutes there and this scan
/// agrees with it exactly.
fn build_pattern(pattern: &str, rewrite: &str) -> VcResult<Pattern> {
    let expando = RustLang.expando_char();
    if pattern.contains(expando) {
        return Err(usage(format!(
            "pattern `{pattern}`: contains `{expando}`, which vc-select substitutes \
             for `$` before parsing — it cannot be matched literally"
        ))
        .with_next("remove the expando character from the pattern"));
    }
    let pat = Pattern::try_new(pattern, RustLang)
        .map_err(|e| usage(format!("pattern `{pattern}`: {e}")))?;
    if pat.has_error() {
        return Err(usage(format!(
            "pattern `{pattern}`: does not parse as rust"
        )));
    }

    let defined = pat.defined_vars();
    let pattern_forms = metavar_forms(pattern);
    let rewrite_forms = metavar_forms(rewrite);
    let template = TemplateFix::try_new(rewrite, &RustLang)
        .map_err(|e| usage(format!("rewrite `{rewrite}`: {e}")))?;
    let mut used: Vec<&str> = template.used_vars().into_iter().collect();
    used.sort_unstable();

    let mut problems: Vec<String> = Vec::new();
    for name in used {
        if !defined.contains(name) {
            problems.push(format!("${name} — the pattern never binds it"));
            continue;
        }
        let bound = pattern_forms.get(name).cloned().unwrap_or_default();
        let wanted = rewrite_forms.get(name).cloned().unwrap_or_default();
        let missing: Vec<Sigil> = wanted
            .iter()
            .copied()
            .filter(|f| !bound.contains(f))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let want = missing
            .iter()
            .map(|f| f.render(name))
            .collect::<Vec<_>>()
            .join(" / ");
        let have = bound
            .iter()
            .map(|f| f.render(name))
            .collect::<Vec<_>>()
            .join(" / ");
        problems.push(if have.is_empty() {
            format!("{want} — the pattern never binds it")
        } else {
            format!("{want} — the pattern binds it as {have}")
        });
    }
    if !problems.is_empty() {
        return Err(
            usage(format!("rewrite `{rewrite}`: {}", problems.join("; ")))
                .with_next("use the same metavariable sigil in the rewrite as in the pattern"),
        );
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
/// * `content_by_path` — the exact bytes of **every scanned file**, not
///   just the ones that produced a site. Hash these, not a second read:
///   the spans are offsets into precisely these buffers, and a scope file
///   that produced no site still has to be described by the bytes this
///   scan saw rather than by whatever is on disk once the scan is over.
///   The one absence is an over-cap file (below) — never read, so never
///   present.
/// * `warnings` — one line per file skipped without failing the run (not
///   UTF-8, its parse tree contains an error, or it is over the size cap).
///   Callers must surface these; a silently skipped file is an incomplete
///   refactor. A file skipped for the first two reasons still has its
///   bytes in `content_by_path` — "we could not match it" is not "we did
///   not read it".
///
/// **Bounded reads.** Each file is opened once and read through
/// `take(`[`velocity_code_kernel::MAX_SEARCH_FILE_BYTES`]` + 1)`, the same
/// discipline `velocity_code_query`'s content search uses. A file already
/// over the cap, and equally one that GROWS past it while being read (the
/// extra probe byte materializes), is skipped with a warning naming it:
/// no buffer is retained and no site is produced for it. The size gate
/// cannot be left to the callers alone, because a caller's `metadata`
/// check and this read are two separate lookups with a window between
/// them — the callers keep their pre-filters for the earlier, friendlier
/// message, and this bound is what makes the racing case safe rather than
/// unbounded.
///
/// **Memory cost.** Retaining every scanned buffer makes the whole scope
/// resident for the life of the call: a scope of N bytes of source costs
/// ~N bytes of heap here, where the previous match-only map cost roughly
/// the size of the matched files alone. That is the price of the
/// single-read discipline — the alternative (re-reading unmatched files
/// later, for the certificate) is a plan-time TOCTOU: a file changed
/// between the match pass and the certificate walk would be baselined
/// *post-change* with no edit, and the apply-time scope-drift check,
/// which compares against that baseline, would be blind to it. The cap is
/// the ceiling on what any ONE file can contribute to that cost.
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
        let Some(bytes) = read_capped(&abs, rel, &mut warnings)? else {
            // Over the cap: not read, so nothing to retain and nothing to
            // match. The warning pushed by `read_capped` is the whole
            // record of it — and for `Plan::build_match` the resulting
            // hole in the content map is a refusal, by design.
            continue;
        };

        // The buffer is retained for EVERY file this scan READ, before any
        // match-side skip decision: a caller's certificate has to describe
        // what this scan saw, and a file we could not match is still a file
        // we read. Inserting here (rather than only on a hit) is what makes
        // the scan the single read of the scope.
        let bytes = content_by_path.entry(rel.clone()).or_insert(bytes);

        let Ok(src) = std::str::from_utf8(bytes) else {
            warnings.push(format!("{}: skipped — not valid utf-8", rel.display()));
            continue;
        };

        match file_sites(rel, src, &pat, rewrite) {
            Ok(Some(found)) => sites.extend(found),
            Ok(None) => {
                warnings.push(format!(
                    "{}: skipped — source did not parse as rust",
                    rel.display()
                ));
            }
            Err(e) => return Err(e),
        }
    }

    sites.sort_by(|a, b| a.path.cmp(&b.path).then(a.start.cmp(&b.start)));
    refuse_overlaps(&sites)?;
    Ok((sites, content_by_path, warnings))
}

/// Read one scope file, bounded at [`MAX_SEARCH_FILE_BYTES`].
///
/// `Ok(None)` means "over the cap — skipped", with the reason pushed onto
/// `warnings`; the caller retains no buffer and produces no sites for it.
/// An unreadable file is still an error (`NotFound`/`Io`), unchanged: a
/// scope file that cannot be opened is the caller's problem to raise, not
/// a gap to warn about.
///
/// Mirrors `velocity_code_query`'s `read_for_search` deliberately, minus
/// the binary sniff (a non-UTF-8 buffer is already handled downstream, and
/// with its own warning). **Open once, then bound.** The handle is opened
/// first and the size read from that same handle, so the gate and the read
/// describe one file rather than two lookups of a path that could have
/// been replaced in between. The size is still only an *opinion about the
/// past* — a file can grow after it is stat'd — so the read is bounded
/// too: `take(MAX + 1)`, and if that extra probe byte materializes the file
/// is treated as over the cap rather than processed truncated. A truncated
/// buffer would be worse than a skip in both directions: it would be
/// matched against as if it were the file, and hashed into a certificate
/// as if it were the file.
fn read_capped(abs: &Path, rel: &Path, warnings: &mut Vec<String>) -> VcResult<Option<Vec<u8>>> {
    let io_err = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => VcError::new(
            ErrorKind::NotFound,
            format!("{}: no such file", rel.display()),
        ),
        _ => VcError::new(ErrorKind::Io, format!("{}: {e}", rel.display())),
    };

    let mut file = std::fs::File::open(abs).map_err(io_err)?;
    let len = file.metadata().map_err(io_err)?.len();
    if len > MAX_SEARCH_FILE_BYTES {
        warnings.push(format!(
            "{}: skipped — {len} bytes exceeds the {MAX_SEARCH_FILE_BYTES}-byte size limit",
            rel.display()
        ));
        return Ok(None);
    }

    let mut bytes = Vec::with_capacity(len as usize);
    (&mut file)
        .take(MAX_SEARCH_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_err)?;
    if bytes.len() as u64 > MAX_SEARCH_FILE_BYTES {
        // The probe byte arrived: the file grew past the cap between the
        // size above and this read — precisely the window a caller's
        // stat-then-reopen pre-filter cannot close. Same policy as a file
        // that was already too big.
        warnings.push(format!(
            "{}: skipped — grew past the {MAX_SEARCH_FILE_BYTES}-byte size limit while being read",
            rel.display()
        ));
        return Ok(None);
    }
    Ok(Some(bytes))
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
        // Skipped for matching, still read — and still in the content map,
        // because a caller's certificate must describe it from the bytes
        // this scan saw rather than from a later, racy re-read.
        assert_eq!(
            content.get(&PathBuf::from("bad.rs")).map(|b| b.as_slice()),
            Some(b"fn broken( { fetch_config(a) ]]]\n".as_slice())
        );
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

        let (sites, content, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["bin.rs"]),
        )
        .unwrap();

        assert!(sites.is_empty());
        assert_eq!(
            content.get(&PathBuf::from("bin.rs")).map(|b| b.as_slice()),
            Some([0xffu8, 0xfe, 0x00].as_slice()),
            "a non-utf8 file is still read once and still returned"
        );
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

    /// Spans are BYTE offsets, not char offsets. The fixture puts two
    /// multi-byte characters *before* the match, so a char-based range lands
    /// three bytes short and `old` comes out as different text entirely. A
    /// multi-byte argument inside the match exercises the rewrite side too.
    #[test]
    fn spans_are_byte_offsets_not_char_offsets() {
        let d = tempfile::tempdir().unwrap();
        // "// café ☕\n" is 10 chars but 13 bytes: é is 2, ☕ is 3.
        let src = "// café ☕\nfn main() { fetch_config(café); }\n";
        write(d.path(), "a.rs", src);

        let (sites, content, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap();

        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert_eq!(sites.len(), 1);

        // The discriminating assertions: literal bytes, and a start that is
        // the BYTE index (25) rather than the char index (22).
        assert_eq!(sites[0].old, "fetch_config(café)".as_bytes());
        assert_eq!(sites[0].new, "load_config(café)".as_bytes());
        let byte_index = src.find("fetch_config").unwrap();
        let char_index = src[..byte_index].chars().count();
        assert_eq!(sites[0].start, byte_index);
        assert_eq!(byte_index, 25);
        assert_eq!(char_index, 22, "char and byte offsets must differ here");
        assert_ne!(sites[0].start, char_index);

        // And the splice still reproduces the whole-document rewrite.
        let buf = &content[&PathBuf::from("a.rs")];
        assert_eq!(buf, src.as_bytes());
        let mut out = Vec::new();
        out.extend_from_slice(&buf[..sites[0].start]);
        out.extend_from_slice(&sites[0].new);
        out.extend_from_slice(&buf[sites[0].end..]);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "// café ☕\nfn main() { load_config(café); }\n"
        );
    }

    /// `$A` and `$$$A` are the same *name* but different bindings, stored in
    /// different halves of ast-grep's `MetaVarEnv`. A name-only guard passes
    /// this and ast-grep then substitutes nothing, silently emitting
    /// `load_config()`. Both directions must refuse.
    #[test]
    fn metavariable_sigil_mismatch_is_usage_in_both_directions() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() { fetch_config(a) }\n");

        // Pattern binds a single node; rewrite asks for a list.
        let e = match_sites(
            d.path(),
            "fetch_config($A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(e.message.contains("$$$A"), "message: {}", e.message);
        assert!(
            e.message.contains("binds it as $A"),
            "both forms must be named: {}",
            e.message
        );

        // Pattern binds a list; rewrite asks for a single node.
        let e = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($A)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(
            e.message.contains("$A — the pattern binds it as $$$A"),
            "both forms must be named: {}",
            e.message
        );

        // Control: matching sigils on both sides are accepted.
        for (p, r) in [
            ("fetch_config($A)", "load_config($A)"),
            ("fetch_config($$$A)", "load_config($$$A)"),
        ] {
            let (sites, _, _) = match_sites(d.path(), p, r, "rust", &scope(&["a.rs"])).unwrap();
            assert_eq!(sites.len(), 1, "{p} -> {r}");
            assert_eq!(sites[0].new, b"load_config(a)", "{p} -> {r}");
        }
    }

    /// `µ` is the expando character the pattern compiler substitutes for `$`,
    /// and `µA` is a legal Rust identifier — so a pattern the caller wrote as
    /// a literal identifier match would silently become a match-anything
    /// capture and rewrite every call site.
    #[test]
    fn expando_character_in_pattern_is_usage() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "a.rs",
            "fn main() { fetch_config(a); other(b); }\n",
        );

        let e = match_sites(
            d.path(),
            "fetch_config(µA)",
            "load_config(µA)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
        assert!(e.message.contains('µ'), "message: {}", e.message);

        // A bare `µ` identifier, with no metavariable shape at all, is
        // refused just the same — the substitution is unconditional.
        let e = match_sites(
            d.path(),
            "fetch_config(µ)",
            "load_config(a)",
            "rust",
            &scope(&["a.rs"]),
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Usage);
    }

    /// The scan must agree with ast-grep's own dollar-run rule, quirks
    /// included, or the sigil guard would refuse or admit the wrong things.
    #[test]
    fn metavar_scan_matches_ast_greps_dollar_run_rule() {
        let forms = |s: &str| {
            metavar_forms(s)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.into_iter().collect::<Vec<_>>()))
                .collect::<Vec<_>>()
        };
        assert_eq!(forms("f($A)"), vec![("A".into(), vec![Sigil::Single])]);
        assert_eq!(forms("f($$$A)"), vec![("A".into(), vec![Sigil::Multi])]);
        // Two dollars is a SINGLE capture in ast-grep, not a list.
        assert_eq!(forms("f($$A)"), vec![("A".into(), vec![Sigil::Single])]);
        // A run with no valid name is literal text.
        assert!(forms("f($)").is_empty());
        assert!(forms("let x = 4;").is_empty());
        // Lowercase is not a metavariable name character.
        assert!(forms("f($a)").is_empty());
        // Both forms of one name are recorded.
        assert_eq!(
            forms("f($A, $$$A)"),
            vec![("A".into(), vec![Sigil::Single, Sigil::Multi])]
        );
        // Multi-byte text around a metavariable does not derail the scan.
        assert_eq!(
            forms("// café ☕ $A"),
            vec![("A".into(), vec![Sigil::Single])]
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

    /// The matcher's OWN size bound — the amendment to "every scanned
    /// buffer is retained", and the case a caller's pre-filter
    /// structurally cannot cover.
    ///
    /// Both CLI callers stat the scope before calling here, but a stat and
    /// this read are two separate lookups with a window between them: a
    /// file can grow past the cap inside it, and before this bound existed
    /// the matcher would then read it whole regardless. Feeding the
    /// over-cap file straight to `match_sites` — the shape a lost race
    /// produces — pins the bound itself rather than the pre-filter.
    ///
    /// Three assertions, and the middle one is the invariant amendment: an
    /// over-cap file yields no site, NO CONTENT-MAP ENTRY (it was never
    /// read, so there are no bytes of it this scan saw, and a truncated or
    /// re-read buffer would put bytes in a certificate that no scan
    /// produced), and a warning naming the file and its size. The missing
    /// entry is exactly what `Plan::build_match` refuses on — see
    /// vc-kernel's `build_match_refuses_a_scope_file_missing_from_the_
    /// content_map`, which pins the other end of that contract.
    #[test]
    fn an_over_cap_file_is_skipped_with_a_warning_and_retains_no_buffer() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() { fetch_config(a) }\n");
        // Sparse (`set_len`), so the fixture costs no real disk — and the
        // gate settles it from the handle's size, so the body is never
        // materialized either.
        let big = std::fs::File::create(d.path().join("big.rs")).unwrap();
        big.set_len(MAX_SEARCH_FILE_BYTES + 1).unwrap();
        drop(big);

        let (sites, content, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs", "big.rs"]),
        )
        .unwrap();

        // The in-cap file is matched exactly as usual: one oversized file
        // in scope must not cost the caller the rest of the answer.
        assert_eq!(sites.len(), 1, "sites: {sites:?}");
        assert_eq!(sites[0].path, PathBuf::from("a.rs"));

        // The over-cap file produced no site and no buffer.
        assert_eq!(
            content.keys().collect::<Vec<_>>(),
            vec![&PathBuf::from("a.rs")],
            "an over-cap file was never read, so it has no content-map entry"
        );

        assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
        assert!(warnings[0].contains("big.rs"), "warning: {}", warnings[0]);
        assert!(
            warnings[0].contains(&(MAX_SEARCH_FILE_BYTES + 1).to_string()),
            "the warning names the size: {}",
            warnings[0]
        );
    }

    /// The single-read property, stated as a map invariant: the content map
    /// is proportional to the SCOPE, not to the result. A file that matched
    /// nothing is still present, holding the exact bytes this scan read —
    /// which is what lets `Plan::build_match` build a provenance certificate
    /// over the whole scope without a second, racy read of the tree.
    #[test]
    fn every_scanned_file_is_in_the_content_map_even_with_no_matches() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.rs", "fn main() { fetch_config(a) }\n");
        write(d.path(), "b.rs", "fn main2() { other() }\n");

        let (sites, content, warnings) = match_sites(
            d.path(),
            "fetch_config($$$A)",
            "load_config($$$A)",
            "rust",
            &scope(&["a.rs", "b.rs"]),
        )
        .unwrap();

        assert_eq!(sites.len(), 1, "only a.rs matches");
        assert!(warnings.is_empty());
        assert_eq!(
            content.keys().collect::<Vec<_>>(),
            vec![&PathBuf::from("a.rs"), &PathBuf::from("b.rs")],
            "both scanned files are in the map, not just the matched one"
        );
        assert_eq!(
            content[&PathBuf::from("b.rs")],
            b"fn main2() { other() }\n".to_vec(),
            "the unmatched file's bytes are exactly what this scan read"
        );
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
