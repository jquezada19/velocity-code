//! `vc` — the velocity-code CLI. Thin by design: parse args, resolve the
//! repo root, dispatch to a handler that builds a [`output::CmdOutcome`]
//! (or fails with a `VcError`), record one metrics line for the
//! invocation regardless of outcome, print per the output contract, exit
//! with the mapped code. All correctness lives in `velocity-code-kernel`
//! and `velocity-code-select`; this crate calls those APIs and never
//! writes a user file itself — only `.vc/metrics/*.jsonl`, which is
//! tool-internal, not user content.

mod metrics;
mod output;

use clap::Parser;
use output::CmdOutcome;
use std::io::Read as _;
use std::path::Path;
use velocity_code_kernel::{
    ErrorKind, VcError, VcResult, lang_tag,
    plan::{MatchSelector, Plan, PlanForm, ResolvedEdit, b64d, b64e},
    recover::{self, DoctorAction},
    resolve::EditRequest,
    {apply, hash, index, root, walk},
};
use velocity_code_select::{edits_from_args, edits_from_diff, match_sites};

#[derive(clap::Parser)]
#[command(name = "vc", version)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    Plan {
        #[command(subcommand)]
        form: PlanCmd,
    },
    Show {
        sha8: String,
    },
    Apply {
        sha8: String,
    },
    Undo {
        id: Option<String>,
    },
    Status,
    Doctor {
        #[arg(long)]
        rollback: bool,
        #[arg(long)]
        discard: bool,
    },
    Gain {
        #[arg(long)]
        history: bool,
    },
    Query {
        pattern: String,
        #[arg(long)]
        regex: bool,
        /// Symbol-name search instead of literal/regex content search
        /// (`vc query NAME --symbol`). Mutually exclusive with `--regex`
        /// and `--ast` — checked in `cmd_query`, not `clap`, so the
        /// refusal routes through the normal `VcError`/`--json` envelope.
        #[arg(long)]
        symbol: bool,
        /// Structural (AST) search instead of literal/regex/symbol search
        /// (`vc query PATTERN --ast`) — the same `ast-grep` engine `plan
        /// match` uses, dry-run: `pattern` is matched with an unused empty
        /// rewrite and every site renders as a query hit at its start
        /// line. Mutually exclusive with `--regex` and `--symbol`. `lang`
        /// is inferred the same way `plan match` infers it (one supported
        /// language present in scope -> use it; a mix -> `Usage`; none ->
        /// `Usage`) unless pinned explicitly.
        #[arg(long)]
        ast: bool,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long)]
        budget: Option<usize>,
        paths: Vec<std::path::PathBuf>,
    },
    Outline {
        path: std::path::PathBuf,
        #[arg(long)]
        budget: Option<usize>,
    },
    Read {
        /// `path[:a-b]` (1-based, inclusive). Omitted when `--symbol` is
        /// given instead — checked in `cmd_read`, not `clap`, so both "no
        /// path and no --symbol" and "both given" route through the normal
        /// `VcError`/`--json` envelope, same pattern as `query`'s
        /// `--symbol`/`--regex` check.
        path: Option<String>,
        #[arg(long)]
        symbol: Option<String>,
        #[arg(long)]
        budget: Option<usize>,
    },
}

#[derive(clap::Subcommand)]
enum PlanCmd {
    Edit {
        file: std::path::PathBuf,
        #[arg(long)]
        old: String,
        #[arg(long)]
        new: String,
    },
    /// Reads a unified diff from stdin.
    Import,
    /// Structural match-and-rewrite plan: `--pattern`/`--rewrite` over
    /// `paths` (empty = whole tree, rebased against the CWD the same way
    /// `plan edit`'s `file` argument is). `--lang` pins the language;
    /// omitted, it's auto-detected from the scope — exactly one supported
    /// language present is used, a mix refuses naming it, none refuses
    /// naming the scope (see [`plan_match_pipeline`]). `--expect N`
    /// refuses (`Usage`, exit 2, nothing stored) unless the matcher finds
    /// exactly N sites.
    Match {
        #[arg(long)]
        pattern: String,
        #[arg(long)]
        rewrite: String,
        #[arg(long)]
        lang: Option<String>,
        #[arg(long)]
        expect: Option<usize>,
        paths: Vec<std::path::PathBuf>,
    },
    /// Re-resolve a stored plan against CURRENT file content and store the
    /// result as a new plan. This is what a `Stale` apply refusal's
    /// `next:` hint points at (I3): the old plan's edits are still exactly
    /// what was asked for, only the file has moved on since it was made,
    /// so refresh re-runs the same resolution fresh rather than asking the
    /// caller to redo the whole `plan edit`/`plan import`/`plan match`
    /// from scratch. For an edit/import plan this replays the stored
    /// edits' old/new text against current content; for a match plan
    /// (`selector: Some(_)`) this instead re-runs the FULL match pipeline
    /// from the stored selector — fresh walk, fresh match — since the
    /// stored edits alone can't reflect a call site that only exists in
    /// the current tree (spec §11b, uniform refresh across all three plan
    /// forms). If the old text no longer exists (or is now ambiguous), the
    /// ordinary resolve error surfaces — that's correct: refresh does not
    /// paper over an edit that no longer applies.
    Refresh { sha8: String },
}

fn verb_name(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Plan { .. } => "plan",
        Cmd::Show { .. } => "show",
        Cmd::Apply { .. } => "apply",
        Cmd::Undo { .. } => "undo",
        Cmd::Status => "status",
        Cmd::Doctor { .. } => "doctor",
        Cmd::Gain { .. } => "gain",
        Cmd::Query { .. } => "query",
        Cmd::Outline { .. } => "outline",
        Cmd::Read { .. } => "read",
    }
}

fn dispatch(root: &Path, cwd: &Path, cmd: &Cmd) -> VcResult<CmdOutcome> {
    match cmd {
        Cmd::Plan { form } => cmd_plan(root, cwd, form),
        Cmd::Show { sha8 } => cmd_show(root, sha8),
        Cmd::Apply { sha8 } => cmd_apply(root, sha8),
        Cmd::Undo { id } => cmd_undo(root, id.as_deref()),
        Cmd::Status => cmd_status(root),
        Cmd::Doctor { rollback, discard } => cmd_doctor(root, *rollback, *discard),
        Cmd::Gain { history } => cmd_gain(root, *history),
        Cmd::Query {
            pattern,
            regex,
            symbol,
            ast,
            lang,
            budget,
            paths,
        } => cmd_query(
            root,
            cwd,
            QueryArgs {
                pattern,
                regex: *regex,
                symbol: *symbol,
                ast: *ast,
                lang: lang.as_deref(),
                budget: *budget,
                paths,
            },
        ),
        Cmd::Outline { path, budget } => cmd_outline(root, cwd, path, *budget),
        Cmd::Read {
            path,
            symbol,
            budget,
        } => cmd_read(root, cwd, path.as_deref(), symbol.as_deref(), *budget),
    }
}

/// Rebase a user-supplied path argument — as typed on the command line,
/// which the shell (and every user's mental model) resolves relative to
/// the process's CWD — onto a root-relative path safe to hand to the
/// kernel. The kernel always resolves edit paths against the repo root,
/// never the CWD (`resolve::resolve_edits` does `root.join(&req.path)`),
/// so without this, `vc plan edit note.txt` run from a subdirectory
/// silently planned against the ROOT's `note.txt` instead of the
/// subdirectory's — a different, wrong file, with no error (C2).
///
/// An absolute path is canonicalized as-is; a relative path is joined
/// onto `cwd` first. Either way the result must canonicalize to somewhere
/// inside `root` (canonicalizing both sides, not a lexical prefix strip,
/// so a `..`-laden relative path or a symlink can't fake containment) —
/// otherwise this refuses (`Usage`, exit 2) instead of silently operating
/// on the wrong file or letting an edit escape the repo.
///
/// Only `plan edit`'s `file` argument goes through this — the only
/// free-standing path argument the CLI takes. `plan import`'s diff-
/// internal paths are deliberately exempt: by unified-diff convention
/// they are already repo-relative (the `a/`/`b/` prefixes name paths from
/// the repo root, not the CWD a diff happens to be generated or piped
/// from), so rebasing them against CWD would be wrong, not a fix.
fn rebase_user_path(root: &Path, cwd: &Path, user_path: &Path) -> VcResult<std::path::PathBuf> {
    let root_real = root
        .canonicalize()
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{}: {e}", root.display())))?;
    let abs = if user_path.is_absolute() {
        user_path.to_path_buf()
    } else {
        cwd.join(user_path)
    };
    // The user's own (relative-as-typed) path names the message here, not
    // `abs` — `abs` is `cwd`-joined and tempdir-prefixed in tests, and
    // leaking that absolute path is both noisy and inconsistent with every
    // other kernel path that reports a missing file (`resolve_edits`'s
    // "{path}: no such file", relative). A `NotFound` canonicalize failure
    // (by far the common case — the file just doesn't exist) is remapped
    // to the same `ErrorKind::NotFound` + message shape the kernel already
    // uses; any other OS error keeps `Io` but still names the user's path.
    let abs_real = abs.canonicalize().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            VcError::new(
                ErrorKind::NotFound,
                format!("{}: no such file", user_path.display()),
            )
        } else {
            VcError::new(ErrorKind::Io, format!("{}: {e}", user_path.display()))
        }
    })?;
    abs_real
        .strip_prefix(&root_real)
        .map(|p| p.to_path_buf())
        .map_err(|_| VcError::new(ErrorKind::Usage, "path outside repo root"))
}

/// `vc plan edit`/`vc plan import`/`vc plan match` -> resolve, digest,
/// store; `vc plan refresh` re-derives one of the three fresh. R1 (ledger
/// ruling): `edit` takes exactly one `--old`/`--new` pair per invocation;
/// a multi-edit plan goes through `import` in M1, repeatable `edit` pairs
/// are M2 polish.
fn cmd_plan(root: &Path, cwd: &Path, form: &PlanCmd) -> VcResult<CmdOutcome> {
    let plan = match form {
        PlanCmd::Edit { file, old, new } => {
            let rel = rebase_user_path(root, cwd, file)?;
            let reqs = edits_from_args(&[(rel, old.clone(), new.clone())]);
            Plan::build(root, PlanForm::Edit, &reqs)?
        }
        PlanCmd::Import => {
            // Diff-internal paths are NOT rebased against cwd — see
            // `rebase_user_path`'s doc comment. A unified diff's `a/`/`b/`
            // paths are repo-relative by convention, independent of
            // wherever the diff itself was generated or piped from.
            let mut diff_text = String::new();
            std::io::stdin().read_to_string(&mut diff_text)?;
            let reqs = edits_from_diff(&diff_text)?;
            Plan::build(root, PlanForm::Import, &reqs)?
        }
        PlanCmd::Match {
            pattern,
            rewrite,
            lang,
            expect,
            paths,
        } => {
            let scope = paths
                .iter()
                .map(|p| rebase_user_path(root, cwd, p))
                .collect::<VcResult<Vec<_>>>()?;
            plan_match_pipeline(root, &scope, lang.as_deref(), pattern, rewrite, *expect)?
        }
        PlanCmd::Refresh { sha8 } => {
            let stale_plan = Plan::load(root, sha8)?;
            match &stale_plan.selector {
                // Match-form: re-run the FULL pipeline from the stored
                // selector (fresh walk, fresh match) instead of replaying
                // stored edits — the stored edits alone can't surface a
                // call site that only exists in the CURRENT tree. No
                // `--expect` on a refresh: the whole point is to accept
                // whatever the current tree now yields.
                Some(sel) => plan_match_pipeline(
                    root,
                    &sel.paths,
                    Some(sel.lang.as_str()),
                    &sel.pattern,
                    &sel.rewrite,
                    None,
                )?,
                // Edit/import-form: unchanged M1 behavior — re-resolve the
                // stored old/new text against current content.
                None => {
                    let reqs = stale_plan
                        .edits
                        .iter()
                        .map(|e| {
                            Ok(EditRequest {
                                path: e.path.clone(),
                                old: b64d(&e.old_b64)?,
                                new: b64d(&e.new_b64)?,
                                // The stored hint, not `None`: an imported
                                // hunk whose old text occurs more than once
                                // was only resolvable because of it, and
                                // dropping it here made such a plan refuse
                                // `Ambiguous` on a tree that never changed.
                                line_hint: e.line_hint,
                            })
                        })
                        .collect::<VcResult<Vec<EditRequest>>>()?;
                    Plan::build(root, stale_plan.form, &reqs)?
                }
            }
        }
    };

    let sha8 = plan.store(root)?;
    let sites = plan.edits.len();
    let files = plan.files.len();
    let epoch8 = index::epoch8(&plan.epoch).to_string();

    let human = format!(
        "plan {sha8} — {sites} sites, {files} files @ epoch {epoch8}   (preview: vc show {sha8})\n"
    );
    let json = serde_json::json!({
        "sha8": sha8,
        "sites": sites,
        "files": files,
        "epoch8": epoch8,
    });
    // Match-form only (`plan.warnings` is always empty on edit/import) —
    // same "join with '; ', print once via the existing CmdOutcome.warning
    // stderr line" convention `apply`/`undo`/`query --symbol`/`read
    // --symbol` already use for their own non-fatal warnings.
    let warning = if plan.warnings.is_empty() {
        None
    } else {
        Some(plan.warnings.join("; "))
    };
    Ok(CmdOutcome {
        human,
        json,
        files,
        edits: sites,
        epoch8,
        warning,
        bytes_out: None,
        naive_bytes: None,
    })
}

/// Fixed priority for the mixed-language refusal message, so `vc plan
/// match`'s auto-detect names the mix in a stable, deterministic order
/// (`"rust+python"`) rather than whatever order a set iterates in.
/// Extending `lang_tag` with a new language just needs a new arm here.
fn lang_priority(lang: &str) -> u8 {
    match lang {
        "rust" => 0,
        "python" => 1,
        _ => 2,
    }
}

/// Render a `plan match` scope for an error message: `.` for the whole
/// tree (empty `paths`), else the given paths joined for display.
fn describe_scope(paths: &[std::path::PathBuf]) -> String {
    if paths.is_empty() {
        ".".to_string()
    } else {
        paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The full `vc plan match` pipeline, shared by `PlanCmd::Match` (fresh
/// `--pattern`/`--rewrite`/scope, `lang` auto-detected unless `--lang` was
/// given, `expect` checked before anything is stored) and
/// `PlanCmd::Refresh`'s match-form path (`sel.paths`/`sel.lang`/
/// `sel.pattern`/`sel.rewrite` from the stored selector, `expect: None`) —
/// refresh re-runs this exact pipeline rather than replaying stored edits,
/// so a refreshed match plan reflects BOTH current file content and
/// current scope membership, uniformly with a fresh `plan match` (spec
/// §11b).
///
/// `lang` — `Some` pins the language outright (an explicit `--lang`, or a
/// refresh's `sel.lang`); `None` auto-detects from the walked scope's
/// `lang_tag`s: exactly one distinct supported language present -> use it;
/// more than one -> `Usage` naming the mix (`"scope spans rust+python —
/// pass --lang"`); none -> `Usage` naming the scope. Returns a fully built
/// (not yet stored) `Plan` — the caller stores it and reports; on an
/// `--expect` mismatch, nothing is built or stored at all.
/// Shared walk + language inference behind both `vc plan match`'s pipeline
/// and `vc query --ast` (Task 15): walks `scope_paths` (empty = whole
/// tree), then either uses the explicit `lang` outright or infers it from
/// the walked scope's `lang_tag`s — one distinct supported language
/// present -> use it; more than one -> `Usage` naming the mix (`"scope
/// spans rust+python — pass --lang"`); none -> `Usage` naming the scope.
/// Returns `(lang, scope_files)`, the lang-filtered file list every
/// caller hands to `match_sites`. Factored out of `plan_match_pipeline`
/// so `query --ast` picks the same language the same way `plan match`
/// does, rather than a second copy of this logic drifting from it.
fn infer_scope_lang(
    root: &Path,
    scope_paths: &[std::path::PathBuf],
    lang: Option<&str>,
) -> VcResult<(String, Vec<std::path::PathBuf>)> {
    let walked = walk::walk_scoped(root, scope_paths)?;

    let lang = match lang {
        Some(l) => l.to_string(),
        None => {
            let mut langs: Vec<&str> = walked
                .iter()
                .map(|p| lang_tag(p))
                .filter(|l| !l.is_empty())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            langs.sort_by_key(|l| lang_priority(l));
            match langs.len() {
                0 => {
                    return Err(VcError::new(
                        ErrorKind::Usage,
                        format!(
                            "{}: scope has no supported language — nothing to match",
                            describe_scope(scope_paths)
                        ),
                    ));
                }
                1 => langs[0].to_string(),
                _ => {
                    return Err(VcError::new(
                        ErrorKind::Usage,
                        format!("scope spans {} — pass --lang", langs.join("+")),
                    ));
                }
            }
        }
    };

    let scope_files: Vec<std::path::PathBuf> =
        walked.into_iter().filter(|p| lang_tag(p) == lang).collect();

    Ok((lang, scope_files))
}

fn plan_match_pipeline(
    root: &Path,
    scope_paths: &[std::path::PathBuf],
    lang: Option<&str>,
    pattern: &str,
    rewrite: &str,
    expect: Option<usize>,
) -> VcResult<Plan> {
    let (lang, scope_files) = infer_scope_lang(root, scope_paths, lang)?;

    let (sites, content_by_path, warnings) =
        match_sites(root, pattern, rewrite, &lang, &scope_files)?;

    if let Some(n) = expect
        && n != sites.len()
    {
        return Err(VcError::new(
            ErrorKind::Usage,
            format!(
                "expected {n} sites, found {} — plan not stored",
                sites.len()
            ),
        ));
    }

    let edits: Vec<ResolvedEdit> = sites
        .into_iter()
        .map(|s| ResolvedEdit {
            path: s.path,
            start: s.start,
            end: s.end,
            old_b64: b64e(&s.old),
            new_b64: b64e(&s.new),
            // A structural match resolves by span, never by a line hint.
            line_hint: None,
        })
        .collect();

    let selector = MatchSelector {
        pattern: pattern.to_string(),
        rewrite: rewrite.to_string(),
        lang,
        paths: scope_paths.to_vec(),
    };

    Plan::build_match(root, selector, edits, &content_by_path, warnings)
}

/// `vc show <sha8>` — full diff preview of a stored plan. The spec pins
/// the `--json` shape (`{sha8, preview}`) but not an exact human string;
/// the natural human rendering of "full diff preview" is the preview text
/// itself, so that's what's printed (see the task report for this call).
fn cmd_show(root: &Path, sha8: &str) -> VcResult<CmdOutcome> {
    let plan = Plan::load(root, sha8)?;
    let preview = plan.preview();
    let sha8_full = plan.sha8();
    let files = plan.files.len();
    let edits = plan.edits.len();
    let epoch8 = index::epoch8(&plan.epoch).to_string();

    // Match-form only (`plan.warnings` is always empty on edit/import):
    // one `warning: {w}` line per matcher warning, appended after the
    // preview — stored on the plan itself (Task 13 controller ruling), so
    // `vc show` on an OLD plan still reports exactly what its selector
    // skipped, not just whatever the `plan match` invocation's own stderr
    // happened to print at the time.
    let mut human = preview.clone();
    for w in &plan.warnings {
        human.push_str(&format!("warning: {w}\n"));
    }

    // `warnings` widens the spec-pinned `{sha8, preview}` `--json`
    // shape, so it appears ONLY when non-empty — mirroring `Plan`'s own
    // `skip_serializing_if` philosophy for
    // `selector`/`certificate`/`warnings` (see plan.rs) rather than
    // always emitting `"warnings": []` for every edit/import plan.
    let mut json = serde_json::json!({
        "sha8": sha8_full,
        "preview": preview,
    });
    if !plan.warnings.is_empty() {
        json["warnings"] = serde_json::json!(plan.warnings);
    }
    Ok(CmdOutcome {
        human,
        json,
        files,
        edits,
        epoch8,
        warning: None,
        bytes_out: None,
        naive_bytes: None,
    })
}

/// Certificate check at apply (Task 14) — the spec's flagship safety
/// scenario: "a 24th site appeared in a file you didn't plan." Runs
/// BEFORE `apply::apply_plan` ever reaches the kernel. A match-form
/// plan's `ProvenanceCert` (`plan.certificate`) records every file its
/// selector could see at plan time (`walk_scoped(selector.paths) ∩
/// lang_tag == selector.lang`, hashed); this re-derives that IDENTICAL
/// candidate set against the CURRENT tree and refuses (`ScopeDrift`, exit
/// 4) if a file OUTSIDE the plan's named set (`plan.files`) now matches
/// the selector's pattern.
///
/// Named-set changes are deliberately NOT this check's concern — a
/// changed NAMED file is the kernel's own stale check (exit 3), which
/// `apply::apply_plan` still runs unconditionally after this returns
/// `Ok(())`. This function only ever refuses; it never authorizes
/// anything the kernel wouldn't already allow (D10), and — since it
/// returns before `apply::apply_plan` is ever called — a refusal here
/// leaves the tree completely untouched.
///
/// **Best-effort in time, not a lock.** This runs BEFORE the journal lock
/// is taken, so a write landing in the window between this check and the
/// apply is not caught here. The kernel's own hash gate — which
/// re-verifies every named file's content under the lock, immediately
/// before writing — remains the authoritative check. What this adds is
/// coverage the kernel structurally cannot have: files OUTSIDE the plan's
/// named set, which the hash gate never looks at. So the window is not a
/// correctness hole; it can only cost a refusal that would have been
/// raised, never grant a write that should have been refused.
///
/// An edit/import plan (form `Edit`/`Import`) skips this entirely: there
/// is no selector to have drifted.
fn check_scope_drift(root: &Path, sha8: &str) -> VcResult<()> {
    let plan = Plan::load(root, sha8)?;
    // Branch on the plan's declared FORM, never on whether the two
    // optional halves happen to be present. An edit/import plan has no
    // selector and is genuinely exempt; a plan that says `Match` and is
    // missing either half is malformed, and must refuse — treating it as
    // "nothing to check" would disarm this guard for exactly the plan
    // shape it exists to police. (`Plan::load` enforces the same
    // invariant, so this is the second of two locks on the same door.)
    let (cert, sel) = match plan.form {
        PlanForm::Edit | PlanForm::Import => return Ok(()),
        PlanForm::Match => match (&plan.certificate, &plan.selector) {
            (Some(cert), Some(sel)) => (cert, sel),
            _ => {
                return Err(VcError::new(
                    ErrorKind::Malformed,
                    format!("plan {sha8}: match-form plan has no selector or certificate"),
                )
                .with_next(format!("vc show {sha8}")));
            }
        },
    };

    // Same walk-then-lang-filter definition `ProvenanceCert::scope_files`
    // was built from (`Plan::build_match`) — the cert's doc comment pins
    // this as binding: both sides must use the identical filter or the
    // comparison below is meaningless.
    let walked = walk::walk_scoped(root, &sel.paths)?;
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for rel in walked {
        if lang_tag(&rel) != sel.lang {
            continue;
        }
        if plan.files.contains_key(&rel) {
            // Named-set changes stay the kernel's stale check (exit 3) —
            // drift is exclusively about files OUTSIDE the plan.
            continue;
        }
        // `file_hash_io` rather than `file_hash` so a read failure's
        // `io::ErrorKind` survives to distinguish the two cases below —
        // `file_hash`'s `?` collapses every I/O error into `ErrorKind::Io`
        // with just the `Display` string, which loses exactly the
        // distinction this needs. It STREAMS: this used to be a whole-file
        // `fs::read` + `bytes_hash`, which made one oversized candidate
        // cost its full length in resident memory just to decide it had
        // not drifted.
        let current_hash = match hash::file_hash_io(&root.join(&rel)) {
            Ok(h) => h,
            // Deleted since plan time: benign — a file that no longer
            // exists cannot contain a new match, so it's simply out of
            // scope now, same as if it had never been in scope.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            // Any OTHER read failure (permissions flipped, transient EIO,
            // ...) must fail CLOSED. A file outside `plan.files` is
            // invisible to the kernel's own stale check, so this is the
            // one place it can be caught at all; skipping it silently
            // would let a file whose current content — and therefore
            // whose match status — is genuinely unknown pass straight
            // through to apply. Refuse instead, the same conservative
            // posture as an unparseable candidate below: a file you
            // cannot read is a file you cannot clear.
            Err(e) => {
                return Err(VcError::new(
                    ErrorKind::ScopeDrift,
                    format!(
                        "{}: {e} — could not be verified against the selector since plan",
                        rel.display()
                    ),
                )
                .with_next(format!("vc plan refresh {sha8}")));
            }
        };
        let drifted = match cert.scope_files.get(&rel) {
            Some(old_hash) => *old_hash != current_hash,
            None => true, // new file — absent from the certificate entirely
        };
        if drifted {
            candidates.push(rel);
        }
    }

    if candidates.is_empty() {
        return Ok(());
    }

    let (sites, _content_by_path, warnings) =
        match_sites(root, &sel.pattern, &sel.rewrite, &sel.lang, &candidates)?;

    // A live match in a drifted, out-of-plan file: the canonical
    // "a site appeared where you weren't looking" refusal. Attribution
    // is strictly per-file: `n` names ONLY the first drifted path's own
    // site count, never the total across every drifted file, which would
    // report a second file's sites against the one the message names.
    // When more than one file drifted, that fact is still surfaced — via
    // a trailing `(+k more file(s))` — without folding their counts
    // into `n`.
    if let Some(first) = sites.first() {
        let mut sites_by_file: std::collections::BTreeMap<&std::path::Path, usize> =
            std::collections::BTreeMap::new();
        for s in &sites {
            *sites_by_file.entry(s.path.as_path()).or_insert(0) += 1;
        }
        let path = first.path.display().to_string();
        let n = sites_by_file[first.path.as_path()];
        let other_files = sites_by_file.len() - 1;
        let mut message = format!("{path} gained a match since plan ({n} new site(s))");
        if other_files > 0 {
            message.push_str(&format!(" (+{other_files} more file(s))"));
        }
        return Err(VcError::new(ErrorKind::ScopeDrift, message)
            .with_next(format!("vc plan refresh {sha8}")));
    }

    // A drifted candidate that `match_sites` could not even parse (or
    // read as valid UTF-8): its match-or-not status is genuinely unknown,
    // and an unknown candidate cannot be cleared by the selector — treat
    // it as drift too (conservative: a file you can't check is a file
    // you can't clear).
    if let Some(w) = warnings.first() {
        return Err(VcError::new(
            ErrorKind::ScopeDrift,
            format!("{w} — could not be verified against the selector since plan"),
        )
        .with_next(format!("vc plan refresh {sha8}")));
    }

    Ok(())
}

fn cmd_apply(root: &Path, sha8: &str) -> VcResult<CmdOutcome> {
    check_scope_drift(root, sha8)?;
    let report = apply::apply_plan(root, sha8)?;
    let epoch8 = index::epoch8(&report.epoch_after).to_string();
    let human = format!(
        "applied: {} edits, {} files. journal {}. undo: vc undo\n",
        report.edits, report.files, report.journal_id
    );
    let json = apply_report_json(&report, &epoch8);
    Ok(CmdOutcome {
        human,
        json,
        files: report.files,
        edits: report.edits,
        epoch8,
        warning: report.warning,
        bytes_out: None,
        naive_bytes: None,
    })
}

fn cmd_undo(root: &Path, id: Option<&str>) -> VcResult<CmdOutcome> {
    let report = apply::undo(root, id)?;
    let epoch8 = index::epoch8(&report.epoch_after).to_string();
    let human = format!(
        "undone: journal {}. files: {}\n",
        report.journal_id, report.files
    );
    let json = apply_report_json(&report, &epoch8);
    Ok(CmdOutcome {
        human,
        json,
        files: report.files,
        edits: report.edits,
        epoch8,
        warning: report.warning,
        bytes_out: None,
        naive_bytes: None,
    })
}

/// Shared `--json` shape for `apply`/`undo` (brief, exact): both return an
/// `apply::ApplyReport`, and only the human line differs between the two
/// verbs (`undo` omits an edit count — `ApplyReport::edits` is always 0
/// for an undo, since undo replays inverse patches rather than resolved
/// edits).
fn apply_report_json(report: &apply::ApplyReport, epoch8: &str) -> serde_json::Value {
    serde_json::json!({
        "journal_id": report.journal_id,
        "files": report.files,
        "edits": report.edits,
        "epoch8_after": epoch8,
        "warning": report.warning,
    })
}

fn cmd_status(root: &Path) -> VcResult<CmdOutcome> {
    let st = recover::status(root)?;
    let journal_head_disp = st
        .journal_head
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let uncommitted_disp = if st.uncommitted.is_empty() {
        "none".to_string()
    } else {
        st.uncommitted.join(",")
    };
    let human = format!(
        "epoch8: {}\nfiles: {}\nplans: {}\njournal_head: {}\nuncommitted: {}\nlock_held: {}\n",
        st.epoch8, st.files, st.plans, journal_head_disp, uncommitted_disp, st.lock_held
    );
    let json = serde_json::json!({
        "epoch8": st.epoch8,
        "files": st.files,
        "plans": st.plans,
        "journal_head": st.journal_head,
        "uncommitted": st.uncommitted,
        "lock_held": st.lock_held,
    });
    let epoch8 = st.epoch8;
    let files = st.files;
    Ok(CmdOutcome {
        human,
        json,
        files,
        edits: 0,
        epoch8,
        warning: None,
        bytes_out: None,
        naive_bytes: None,
    })
}

/// Doctor flag mapping (brief, exact): neither flag -> `Report`;
/// `--rollback` -> `Rollback`; `--discard` -> `Discard`; both -> usage
/// error, exit 2. Checked here (not via clap `conflicts_with`) so the
/// refusal routes through the same VcError/`--json` envelope as every
/// other error instead of clap's own hard-coded usage message.
fn cmd_doctor(root: &Path, rollback: bool, discard: bool) -> VcResult<CmdOutcome> {
    let action = match (rollback, discard) {
        (false, false) => DoctorAction::Report,
        (true, false) => DoctorAction::Rollback,
        (false, true) => DoctorAction::Discard,
        (true, true) => {
            return Err(VcError::new(
                ErrorKind::Usage,
                "doctor: --rollback and --discard are mutually exclusive",
            ));
        }
    };
    let rep = recover::doctor(root, action)?;
    let human = format!(
        "doctor: rolled_back={:?} discarded={:?} lock_removed={} healthy={}\n",
        rep.rolled_back, rep.discarded, rep.lock_removed, rep.healthy
    );
    let json = serde_json::json!({
        "rolled_back": rep.rolled_back,
        "discarded": rep.discarded,
        "lock_removed": rep.lock_removed,
        "healthy": rep.healthy,
    });
    Ok(CmdOutcome {
        human,
        json,
        files: 0,
        edits: 0,
        epoch8: String::new(),
        warning: None,
        bytes_out: None,
        naive_bytes: None,
    })
}

fn cmd_gain(root: &Path, history: bool) -> VcResult<CmdOutcome> {
    let report = metrics::aggregate(root, history);
    let human = metrics::format_human(&report);
    let json = metrics::to_json(&report);
    Ok(CmdOutcome {
        human,
        json,
        files: 0,
        edits: 0,
        epoch8: String::new(),
        warning: None,
        bytes_out: None,
        naive_bytes: None,
    })
}

/// Sum of the on-disk sizes of every distinct file in `files` — the
/// read-side gain accounting's `naive_bytes` counterfactual for a query
/// mode (spec §7.2): "what would it have cost to just read every file
/// that contributed a hit." An unreadable/vanished file (a race between
/// the search read and this stat) contributes `0` rather than failing the
/// whole command — this is observability, not correctness, same posture
/// `metrics::record` itself takes.
fn naive_bytes_for_files<'a>(
    root: &Path,
    files: impl IntoIterator<Item = &'a std::path::PathBuf>,
) -> u64 {
    files
        .into_iter()
        .map(|p| {
            std::fs::metadata(root.join(p))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum()
}

/// `vc query <PATTERN> [--regex] [--symbol] [--ast] [--budget N]
/// [paths…]` — read-only search, never touches a user file. `paths`
/// (empty = whole tree) are rebased per-argument through the same
/// [`rebase_user_path`] `plan edit` uses, so a scope path is interpreted
/// relative to the CWD the same way a `plan edit` file argument is, and a
/// path escaping `root` refuses the same way (`Usage`, exit 2). The epoch
/// stamp comes from a fresh `index::refresh` — unlike `plan`/`apply`,
/// `query` has no stored plan to read an epoch off, so it takes the live
/// one directly, same source `vc status` reads. Zero hits is success
/// (exit 0), not an error: an agent needs to tell "found nothing" apart
/// from "the command failed."
///
/// `--symbol`, `--regex`, and `--ast` are three separate search modes,
/// mutually exclusive with each other — checked here (not via `clap`'s
/// `conflicts_with`) so the refusal routes through the same
/// `VcError`/`--json` envelope as every other error, same pattern as
/// `cmd_doctor`'s `--rollback`/`--discard` check. `--ast` switches to
/// structural search (`cmd_query_ast`), the same `ast-grep` engine `plan
/// match` uses — literally the dry-run of the edit.
fn cmd_query(root: &Path, cwd: &Path, args: QueryArgs<'_>) -> VcResult<CmdOutcome> {
    let QueryArgs {
        pattern,
        regex,
        symbol,
        ast,
        lang,
        budget,
        paths,
    } = args;
    if [symbol, regex, ast].iter().filter(|&&b| b).count() > 1 {
        return Err(VcError::new(
            ErrorKind::Usage,
            "query: --symbol, --regex, and --ast are mutually exclusive",
        ));
    }

    let scope = paths
        .iter()
        .map(|p| rebase_user_path(root, cwd, p))
        .collect::<VcResult<Vec<_>>>()?;

    let (_ix, epoch) = index::refresh(root)?;
    let epoch8 = index::epoch8(&epoch).to_string();

    if symbol {
        return cmd_query_symbol(root, pattern, budget, &scope, epoch8);
    }
    if ast {
        return cmd_query_ast(root, pattern, lang, budget, &scope, epoch8);
    }

    let (hits, warnings) = if regex {
        velocity_code_query::search_regex(root, pattern, &scope)?
    } else {
        velocity_code_query::search_literal(root, pattern, &scope)?
    };
    Ok(query_hits_outcome(root, &hits, budget, epoch8, warnings))
}

/// `vc query`'s parsed arguments, carried as one named value rather than
/// seven positional ones. Three of them are bare `bool`s that a caller
/// could transpose without the compiler noticing — and transposing
/// `symbol` with `ast` silently changes which search mode runs.
struct QueryArgs<'a> {
    pattern: &'a str,
    regex: bool,
    symbol: bool,
    ast: bool,
    lang: Option<&'a str>,
    budget: Option<usize>,
    paths: &'a [std::path::PathBuf],
}

/// The shared tail of every `Vec<QueryHit>` search mode — literal, regex
/// (`cmd_query`) and structural (`cmd_query_ast`): epoch header, budgeted
/// render, the elided line, the `--json` envelope, read-gain accounting,
/// and the joined warning line. All three modes differ only in how they
/// PRODUCE hits; from the hits onward they were three verbatim copies of
/// this, which is three places for one contract to drift.
///
/// `header_suffix` is the one point of variation the callers actually
/// need (`query --symbol` marks a fuzzy result there); it is appended to
/// the `— N hits` header.
fn query_hits_outcome(
    root: &Path,
    hits: &[velocity_code_query::QueryHit],
    budget: Option<usize>,
    epoch8: String,
    warnings: Vec<String>,
) -> CmdOutcome {
    let budgeted = velocity_code_query::render_hits(hits, budget);
    let (human, included) = render_hits_human(&epoch8, "", hits.len(), &budgeted);

    // `render_hits` walks `hits` front-to-back, greedily including whole
    // hits until the running token estimate would exceed `budget`, so the
    // hits it actually rendered into `budgeted.text` are exactly the first
    // `hits.len() - budgeted.elided` of them — slicing here keeps the
    // `--json` `hits` array consistent with `elided` (their counts must
    // sum to the total match count) instead of dumping every match
    // regardless of budget.
    let json_hits: Vec<serde_json::Value> = hits[..included]
        .iter()
        .map(|h| {
            serde_json::json!({
                "path": h.path.to_string_lossy(),
                "line": h.line,
                "col": h.col,
                "text": h.line_text,
            })
        })
        .collect();
    let json = serde_json::json!({
        "epoch8": epoch8,
        "hits": json_hits,
        "elided": budgeted.elided,
    });

    let unique_files: std::collections::BTreeSet<&std::path::PathBuf> =
        hits.iter().map(|h| &h.path).collect();
    let files = unique_files.len();
    let bytes_out = human.len() as u64;
    let naive_bytes = naive_bytes_for_files(root, unique_files);

    CmdOutcome {
        human,
        json,
        files,
        edits: hits.len(),
        epoch8,
        warning: join_warnings(warnings),
        bytes_out: Some(bytes_out),
        naive_bytes: Some(naive_bytes),
    }
}

/// The `epoch {e} — {n} hits{suffix}` header plus the budgeted body and
/// the `… elided N hits (budget)` line, shared by every query mode.
/// Returns the rendered text and the number of hits it actually included
/// (total minus elided) so the caller can slice `--json` to match.
fn render_hits_human(
    epoch8: &str,
    header_suffix: &str,
    total: usize,
    budgeted: &velocity_code_query::Budgeted,
) -> (String, usize) {
    let mut human = format!("epoch {epoch8} — {total} hits{header_suffix}\n");
    if !budgeted.text.is_empty() {
        human.push_str(&budgeted.text);
        human.push('\n');
    }
    if budgeted.elided > 0 {
        human.push_str(&format!("… elided {} hits (budget)\n", budgeted.elided));
    }
    (human, total - budgeted.elided)
}

/// Multiple per-file warnings collapse into `CmdOutcome`'s single
/// `warning` slot (the same one apply/undo use for a non-fatal,
/// surfaced-but-not-failing condition) — joined rather than dropped, so
/// every skipped file is still visible to the caller. `None` when there is
/// nothing to say, so no empty `warning:` line is printed.
fn join_warnings(warnings: Vec<String>) -> Option<String> {
    if warnings.is_empty() {
        None
    } else {
        Some(warnings.join("; "))
    }
}

/// `vc query NAME --symbol` handler, split out of [`cmd_query`] because its
/// hit shape (`SymbolHit`, no `col`/`line_text`) and `--json` shape (adds
/// `fuzzy`, `kind`/`signature` instead of `col`/`text`, and a per-hit
/// `fuzzy_source` only in fuzzy mode) genuinely diverge from the literal/
/// regex path rather than just needing a branch inside it. `epoch8` is
/// passed in already computed — `cmd_query` takes it from the same
/// `index::refresh` call both paths need for the header, before branching.
fn cmd_query_symbol(
    root: &Path,
    name: &str,
    budget: Option<usize>,
    scope: &[std::path::PathBuf],
    epoch8: String,
) -> VcResult<CmdOutcome> {
    let (hits, fuzzy, warnings) = velocity_code_query::search_symbol(root, name, scope)?;
    let n = hits.len();
    let budgeted = velocity_code_query::render_symbol_hits(&hits, budget);

    // `--json` has carried a top-level `fuzzy` flag since this mode
    // shipped; the human header did not, so a human reader saw a plain
    // "N hits" for a result that contains no exact match at all — every
    // hit merely a substring neighbour of what they asked for. Mark it.
    // (Marked only when there is something to mark: a zero-hit search is
    // reported as fuzzy by `search_symbol`'s tiering, but "0 hits (fuzzy:
    // no exact match)" would put the label on every empty result.)
    let header_suffix = if fuzzy && n > 0 {
        " (fuzzy: no exact match)"
    } else {
        ""
    };
    // Same "included = total - elided, slice the front" reasoning as
    // cmd_query's literal/regex path: render_symbol_hits walks hits
    // front-to-back and greedily includes whole hits, so the rendered
    // text is exactly the first `n - budgeted.elided` of them.
    let (human, included) = render_hits_human(&epoch8, header_suffix, n, &budgeted);

    let json_hits: Vec<serde_json::Value> = hits[..included]
        .iter()
        .map(|h| {
            let mut obj = serde_json::Map::new();
            obj.insert("path".into(), serde_json::json!(h.path.to_string_lossy()));
            obj.insert("line".into(), serde_json::json!(h.symbol.start_line));
            obj.insert(
                "kind".into(),
                serde_json::json!(velocity_code_query::symbol_kind_label(&h.symbol.kind)),
            );
            obj.insert("signature".into(), serde_json::json!(h.symbol.signature));
            // fuzzy_source is redundant in exact mode (the symbol's name
            // already equals the query) so it's only emitted when the
            // match came from the fuzzy substring fallback.
            if fuzzy {
                obj.insert("fuzzy_source".into(), serde_json::json!(h.symbol.name));
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    let json = serde_json::json!({
        "epoch8": epoch8,
        "hits": json_hits,
        "fuzzy": fuzzy,
        "elided": budgeted.elided,
    });

    let unique_files: std::collections::BTreeSet<&std::path::PathBuf> =
        hits.iter().map(|h| &h.path).collect();
    let files = unique_files.len();
    let bytes_out = human.len() as u64;
    let naive_bytes = naive_bytes_for_files(root, unique_files);

    Ok(CmdOutcome {
        human,
        json,
        files,
        edits: n,
        epoch8,
        warning: join_warnings(warnings),
        bytes_out: Some(bytes_out),
        naive_bytes: Some(naive_bytes),
    })
}

/// `vc query PATTERN --ast [--lang L]` handler, split out of [`cmd_query`]
/// for the same reason `cmd_query_symbol` is: its hit source (structural
/// match sites, not a line/regex scan) and language-inference step
/// genuinely diverge from the literal/regex path. `lang` resolution goes
/// through [`infer_scope_lang`] — the exact function `plan_match_pipeline`
/// uses — so `--ast`'s auto-detect and refusal messages match `plan
/// match`'s precisely rather than a second, drifting copy. From the hits
/// onward it shares [`query_hits_outcome`] with the literal/regex path —
/// same rendering, budgeting, `--json` shape, read-gain accounting, and
/// warning surfacing, because both work over `Vec<QueryHit>` and a file
/// `match_sites` had to skip must not fail a query that still found real
/// sites.
fn cmd_query_ast(
    root: &Path,
    pattern: &str,
    lang: Option<&str>,
    budget: Option<usize>,
    scope: &[std::path::PathBuf],
    epoch8: String,
) -> VcResult<CmdOutcome> {
    let (lang, scope_files) = infer_scope_lang(root, scope, lang)?;
    let (hits, warnings) = velocity_code_query::search_ast(root, pattern, &lang, &scope_files)?;
    Ok(query_hits_outcome(root, &hits, budget, epoch8, warnings))
}

/// `vc outline <path> [--budget N]` — read-only skeleton render, never
/// touches a user file. `path` is rebased through the same
/// [`rebase_user_path`] `plan edit`'s `file` argument uses, so it's
/// interpreted relative to the CWD, not the repo root. The epoch stamp
/// comes from a fresh `index::refresh`, matching `query`'s convention (no
/// stored plan to read one off). Language is resolved from the
/// root-relative path's extension via `lang_tag` — the same tag the stat
/// index itself records — so an unsupported extension refuses through
/// `velocity_code_lang::outline::outline`'s own `Usage` error rather than
/// silently returning an empty skeleton.
fn cmd_outline(
    root: &Path,
    cwd: &Path,
    path: &std::path::Path,
    budget: Option<usize>,
) -> VcResult<CmdOutcome> {
    let rel = rebase_user_path(root, cwd, path)?;
    let (_ix, epoch) = index::refresh(root)?;
    let epoch8 = index::epoch8(&epoch).to_string();

    let path_disp = rel.display().to_string();
    let src = std::fs::read_to_string(root.join(&rel))
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{path_disp}: {e}")))?;
    let lang = lang_tag(&rel);
    // `outline` knows the language is unsupported but not which file was
    // asked for, so it states the fact and leaves `next:` empty; this is
    // the layer that can name the path in a command the caller can run.
    let (skeleton, elided) =
        velocity_code_lang::outline::outline(&src, lang, budget).map_err(|e| {
            if e.next.is_none() {
                e.with_next(format!("vc read {path_disp}"))
            } else {
                e
            }
        })?;

    let mut human = format!("epoch {epoch8} — {elided} elided\n");
    if !skeleton.is_empty() {
        human.push_str(&skeleton);
        human.push('\n');
    }

    let json = serde_json::json!({
        "epoch8": epoch8,
        "outline": skeleton,
        "elided": elided,
    });

    let bytes_out = human.len() as u64;
    let naive_bytes = src.len() as u64;

    Ok(CmdOutcome {
        human,
        json,
        files: 1,
        edits: 0,
        epoch8,
        warning: None,
        bytes_out: Some(bytes_out),
        naive_bytes: Some(naive_bytes),
    })
}

/// `vc read <path[:a-b] | --symbol NAME> [--budget N]` — read-only,
/// mutually exclusive path/`--symbol` modes checked here (not `clap`'s
/// `conflicts_with`) for the same reason `query`'s `--symbol`/`--regex`
/// check is: the refusal routes through the normal `VcError`/`--json`
/// envelope.
fn cmd_read(
    root: &Path,
    cwd: &Path,
    path: Option<&str>,
    symbol: Option<&str>,
    budget: Option<usize>,
) -> VcResult<CmdOutcome> {
    match (path, symbol) {
        (Some(_), Some(_)) => Err(VcError::new(
            ErrorKind::Usage,
            "read: <path> and --symbol are mutually exclusive",
        )),
        (None, None) => Err(VcError::new(
            ErrorKind::Usage,
            "read: pass a path, or --symbol NAME",
        )),
        (Some(p), None) => cmd_read_path(root, cwd, p, budget),
        (None, Some(name)) => cmd_read_symbol(root, name, budget),
    }
}

/// Splits a `read` path argument on its trailing `:a-b` range suffix, if
/// any. Only a suffix that actually parses as two dash-separated `usize`s
/// counts as a range — anything else (no colon, or a colon that isn't
/// followed by a valid range) is treated as a plain path with no range,
/// which also keeps a path containing an unrelated `:` from being
/// misparsed.
fn parse_range_suffix(s: &str) -> (&str, Option<(usize, usize)>) {
    if let Some((file, range)) = s.rsplit_once(':')
        && let Some((a, b)) = range.split_once('-')
        && let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>())
    {
        return (file, Some((a, b)));
    }
    (s, None)
}

/// File/range `read` mode: exact requested lines, each prefixed `{line}:
/// `. `:a-b` is 1-based inclusive; `a == 0` or `a > b` refuses `Usage`
/// before ever touching the file. `b` beyond EOF clamps to the file's true
/// last line rather than refusing — agents overshoot ranges constantly, and
/// a clamp (with the true end reported back) is more useful than a
/// refusal.
///
/// A start line beyond EOF is the other half, and is NOT clamped: `a >
/// total` names no content at all, so it refuses `NotFound` naming the
/// file's true length, with a `next:` hint for the whole file. Returning
/// an inverted `start > end` "success" with empty text would hide from the
/// caller that they read nothing.
///
/// No range at all reads the whole file. Over budget (when set) refuses
/// via [`cmd_read_budget_check`] instead of silently truncating.
fn cmd_read_path(
    root: &Path,
    cwd: &Path,
    arg: &str,
    budget: Option<usize>,
) -> VcResult<CmdOutcome> {
    let (file_str, range) = parse_range_suffix(arg);
    if let Some((a, b)) = range
        && (a == 0 || a > b)
    {
        return Err(VcError::new(
            ErrorKind::Usage,
            format!("read: invalid range {a}-{b}"),
        ));
    }

    let rel = rebase_user_path(root, cwd, Path::new(file_str))?;
    let (_ix, epoch) = index::refresh(root)?;
    let epoch8 = index::epoch8(&epoch).to_string();

    let path_disp = rel.display().to_string();
    let abs = root.join(&rel);

    // Open ONCE, and take the size from that handle. The budget pre-check
    // and the read that follows then describe the same open file, rather
    // than two independent lookups of a path that could name a different
    // inode by the second one — a `--budget 200` pre-check that cleared a
    // small file, followed by a read of a large replacement, would have
    // been the failure this shape removes.
    //
    // Residual, stated precisely: the check still describes the file as it
    // was when `metadata` was called on the handle. The SAME file can grow
    // between that call and the `read_to_string` below, so a whole-file
    // read can still return more than the pre-check reasoned about. That
    // growth is out of scope here and is not a correctness hole: the
    // post-render `cmd_read_budget_check` in `read_outcome` runs on the
    // actually-rendered text and still refuses. What the open-once shape
    // rules out is the different-inode case, which no later check catches.
    let mut file = std::fs::File::open(&abs)
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{path_disp}: {e}")))?;

    // Whole-file read with a budget: settle it from the file's SIZE before
    // reading a byte. A budget refusal on the whole file is decided by
    // `tokens_est` over the rendered text, and the rendered text is the
    // content plus a `{line}: ` prefix per line — never shorter than the
    // raw bytes. So a file whose raw length already blows the budget can
    // only refuse, and reading it in full first (a `--budget 200` read of
    // a 2 GiB file materialized all 2 GiB, then refused) buys nothing.
    //
    // Deliberately not applied to a RANGE read: a small range out of a
    // large file is a legitimate, useful request, and gating it on the
    // whole file's size would turn working reads into refusals. The
    // post-render check below still covers that case exactly as before.
    if range.is_none()
        && let Some(budget) = budget
        && let Ok(md) = file.metadata()
    {
        cmd_read_budget_check(&path_disp, md.len() as usize, Some(budget))?;
    }

    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{path_disp}: {e}")))?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let (start, end) = match range {
        Some((a, b)) if a > total => {
            return Err(VcError::new(
                ErrorKind::NotFound,
                format!("{path_disp}:{a}-{b}: start beyond EOF ({total} lines)"),
            )
            .with_next(format!("vc read {path_disp}:1-{total}")));
        }
        Some((a, b)) => (a, b.min(total)),
        None => (1, total),
    };

    read_outcome(ReadRender {
        path_disp: &path_disp,
        lines: &lines,
        start,
        end,
        naive_bytes: content.len() as u64,
        epoch8,
        budget,
        warning: None,
    })
}

/// `--symbol NAME` `read` mode: unique EXACT match -> its full body, line-
/// prefixed the same way `cmd_read_path` renders a range. Zero matches ->
/// `NotFound`; more than one -> `Ambiguous`, listing every candidate as
/// `path:line` in the message so the caller can retry with an explicit
/// range on the one they meant.
///
/// A FUZZY-only result refuses. `search_symbol` falls back to a
/// case-insensitive substring tier when no symbol's name equals the query,
/// and `read` used to discard that flag entirely — so `vc read --symbol
/// load_config`, with no `load_config` anywhere in the tree, printed
/// `load_configuration_from_disk`'s body at exit 0 with nothing in the
/// output marking it as a different function. Serving the wrong body,
/// unlabelled, to a caller who asked for a specific one is the failure
/// this whole tool exists to refuse. The candidates are listed (`path:line
/// name`) so the near-misses are still useful, and the `next:` hint points
/// at `vc query --symbol`, which is the verb that may legitimately answer
/// fuzzily.
fn cmd_read_symbol(root: &Path, name: &str, budget: Option<usize>) -> VcResult<CmdOutcome> {
    let (_ix, epoch) = index::refresh(root)?;
    let epoch8 = index::epoch8(&epoch).to_string();

    let (hits, fuzzy, warnings) = velocity_code_query::search_symbol(root, name, &[])?;
    let hit = match hits.len() {
        0 => {
            return Err(
                VcError::new(ErrorKind::NotFound, format!("{name}: no symbol found"))
                    .with_next(format!("vc query {name} --symbol")),
            );
        }
        _ if fuzzy => {
            let candidates = hits
                .iter()
                .map(|h| {
                    format!(
                        "{}:{} {}",
                        h.path.display(),
                        h.symbol.start_line,
                        h.symbol.name
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(VcError::new(
                ErrorKind::NotFound,
                format!("{name}: no symbol by that name; did you mean: {candidates}"),
            )
            .with_next(format!("vc query {name} --symbol")));
        }
        1 => &hits[0],
        n => {
            let candidates = hits
                .iter()
                .map(|h| format!("{}:{}", h.path.display(), h.symbol.start_line))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(VcError::new(
                ErrorKind::Ambiguous,
                format!("{name}: {n} symbols match: {candidates}"),
            ));
        }
    };

    let path_disp = hit.path.display().to_string();
    let content = std::fs::read_to_string(root.join(&hit.path))
        .map_err(|e| VcError::new(ErrorKind::Io, format!("{path_disp}: {e}")))?;
    let lines: Vec<&str> = content.lines().collect();
    let start = hit.symbol.start_line;
    let end = hit.symbol.end_line.min(lines.len());

    // Same non-fatal-warning posture as `cmd_query_symbol`: a malformed
    // file elsewhere in the search must not fail a read that found its
    // target fine.
    read_outcome(ReadRender {
        path_disp: &path_disp,
        lines: &lines,
        start,
        end,
        naive_bytes: content.len() as u64,
        epoch8,
        budget,
        warning: join_warnings(warnings),
    })
}

/// What a `read` resolved to, ready to render. The two modes differ only
/// in how they arrive at a path and a line range; from here on they are
/// the same command. Named fields rather than a positional list because
/// `start`/`end` are adjacent same-typed numbers and the whole point of
/// this verb is returning exactly the lines that were asked for.
struct ReadRender<'a> {
    path_disp: &'a str,
    lines: &'a [&'a str],
    start: usize,
    end: usize,
    /// The full file's byte length — the read-gain counterfactual ("what
    /// reading the whole thing would have cost"), not the size of what is
    /// actually returned.
    naive_bytes: u64,
    epoch8: String,
    budget: Option<usize>,
    warning: Option<String>,
}

/// The shared tail of both `read` modes: render lines `start..=end` with
/// their `{line}: ` prefixes, apply the budget refusal, then build the
/// identical human text, `--json` shape (`{epoch8, path, start, end,
/// text}`) and read-gain accounting.
fn read_outcome(r: ReadRender<'_>) -> VcResult<CmdOutcome> {
    let ReadRender {
        path_disp,
        lines,
        start,
        end,
        naive_bytes,
        epoch8,
        budget,
        warning,
    } = r;

    let mut text = String::new();
    for i in start..=end {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("{i}: {}", lines[i - 1]));
    }

    cmd_read_budget_check(path_disp, text.len(), budget)?;

    let mut human = format!("epoch {epoch8} — {path_disp}:{start}-{end}\n");
    if !text.is_empty() {
        human.push_str(&text);
        human.push('\n');
    }
    let json = serde_json::json!({
        "epoch8": epoch8,
        "path": path_disp,
        "start": start,
        "end": end,
        "text": text,
    });

    let bytes_out = human.len() as u64;

    Ok(CmdOutcome {
        human,
        json,
        files: 1,
        edits: 0,
        epoch8,
        warning,
        bytes_out: Some(bytes_out),
        naive_bytes: Some(naive_bytes),
    })
}

/// Shared over-budget refusal for both `read` modes: `budget: None` always
/// passes. Otherwise, refuses with `ErrorKind::Budget` — a budget refusal
/// is a fact about the requested *content*, not a malformed invocation, so
/// it is deliberately not `Usage` — the moment `bytes`' `tokens_est` would
/// exceed it, before any of that content reaches the caller. `read` never
/// silently truncates.
///
/// Takes a byte COUNT rather than the text itself so a whole-file read can
/// settle the question from `metadata` without materializing the file: the
/// rendered text is never shorter than the raw content it wraps, so a raw
/// length already over budget can only refuse.
fn cmd_read_budget_check(path_disp: &str, bytes: usize, budget: Option<usize>) -> VcResult<()> {
    let Some(budget) = budget else {
        return Ok(());
    };
    let tokens = velocity_code_query::tokens_est(bytes);
    if tokens > budget {
        return Err(VcError::new(
            ErrorKind::Budget,
            format!("{path_disp} is ~{tokens} tokens (budget {budget})"),
        )
        .with_next(format!("vc outline {path_disp}")));
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("io: failed to read current directory: {e}");
        std::process::exit(1);
    });
    // Root discovery can itself refuse (a symlinked `.vc` — B5/Toctou):
    // routed through the same `output::emit` the rest of the CLI uses, so
    // a refusal here gets the identical human/`--json` error envelope and
    // exit code as any other `VcError`, just before there's a repo root
    // to record metrics against.
    let repo_root = match root::find_root(&cwd) {
        Ok(p) => p,
        Err(e) => {
            let code = output::emit(cli.json, &Err(e));
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            std::process::exit(code);
        }
    };

    let verb = verb_name(&cli.cmd);
    let start = std::time::Instant::now();
    let result = dispatch(&repo_root, &cwd, &cli.cmd);
    let ms = start.elapsed().as_millis() as u64;

    let (files, edits, epoch8, refusal, bytes_out, naive_bytes) = match &result {
        Ok(o) => (
            o.files,
            o.edits,
            o.epoch8.clone(),
            None,
            o.bytes_out,
            o.naive_bytes,
        ),
        Err(e) => (
            0,
            0,
            String::new(),
            Some(output::error_kind_label(e.kind)),
            None,
            None,
        ),
    };
    metrics::record(
        &repo_root,
        &metrics::MetricEvent {
            verb,
            ms,
            files,
            edits,
            refusal,
            epoch8: &epoch8,
            bytes_out,
            naive_bytes,
        },
    );

    let code = output::emit(cli.json, &result);
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
