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
    ErrorKind, VcError, VcResult,
    plan::{Plan, PlanForm, b64d},
    recover::{self, DoctorAction},
    resolve::EditRequest,
    {apply, index, root},
};
use velocity_code_select::{edits_from_args, edits_from_diff};

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
        #[arg(long)]
        budget: Option<usize>,
        paths: Vec<std::path::PathBuf>,
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
    /// Re-resolve a stored plan's edits against CURRENT file content and
    /// store the result as a new plan. This is what a `Stale` apply
    /// refusal's `next:` hint points at (I3): the old plan's edits are
    /// still exactly what was asked for, only the file has moved on since
    /// it was made, so refresh re-runs the same resolution fresh rather
    /// than asking the caller to redo the whole `plan edit`/`plan import`
    /// from scratch. If the old text no longer exists (or is now
    /// ambiguous), the ordinary resolve error surfaces — that's correct:
    /// refresh does not paper over an edit that no longer applies.
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
            budget,
            paths,
        } => cmd_query(root, cwd, pattern, *regex, *budget, paths),
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

/// `vc plan edit`/`vc plan import` -> resolve, digest, store. R1 (ledger
/// ruling): `edit` takes exactly one `--old`/`--new` pair per invocation;
/// a multi-edit plan goes through `import` in M1, repeatable `edit` pairs
/// are M2 polish.
fn cmd_plan(root: &Path, cwd: &Path, form: &PlanCmd) -> VcResult<CmdOutcome> {
    let (plan_form, reqs) = match form {
        PlanCmd::Edit { file, old, new } => {
            let rel = rebase_user_path(root, cwd, file)?;
            let reqs = edits_from_args(&[(rel, old.clone(), new.clone())]);
            (PlanForm::Edit, reqs)
        }
        PlanCmd::Import => {
            // Diff-internal paths are NOT rebased against cwd — see
            // `rebase_user_path`'s doc comment. A unified diff's `a/`/`b/`
            // paths are repo-relative by convention, independent of
            // wherever the diff itself was generated or piped from.
            let mut diff_text = String::new();
            std::io::stdin().read_to_string(&mut diff_text)?;
            let reqs = edits_from_diff(&diff_text)?;
            (PlanForm::Import, reqs)
        }
        PlanCmd::Refresh { sha8 } => {
            let stale_plan = Plan::load(root, sha8)?;
            let reqs = stale_plan
                .edits
                .iter()
                .map(|e| {
                    Ok(EditRequest {
                        path: e.path.clone(),
                        old: b64d(&e.old_b64)?,
                        new: b64d(&e.new_b64)?,
                        line_hint: None,
                    })
                })
                .collect::<VcResult<Vec<EditRequest>>>()?;
            (stale_plan.form, reqs)
        }
    };

    let plan = Plan::build(root, plan_form, &reqs)?;
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
    Ok(CmdOutcome {
        human,
        json,
        files,
        edits: sites,
        epoch8,
        warning: None,
    })
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

    let json = serde_json::json!({
        "sha8": sha8_full,
        "preview": preview,
    });
    Ok(CmdOutcome {
        human: preview,
        json,
        files,
        edits,
        epoch8,
        warning: None,
    })
}

fn cmd_apply(root: &Path, sha8: &str) -> VcResult<CmdOutcome> {
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
    })
}

/// `vc query <PATTERN> [--regex] [--budget N] [paths…]` — read-only search,
/// never touches a user file. `paths` (empty = whole tree) are rebased
/// per-argument through the same [`rebase_user_path`] `plan edit` uses, so
/// a scope path is interpreted relative to the CWD the same way a `plan
/// edit` file argument is, and a path escaping `root` refuses the same way
/// (`Usage`, exit 2). The epoch stamp comes from a fresh
/// `index::refresh` — unlike `plan`/`apply`, `query` has no stored plan to
/// read an epoch off, so it takes the live one directly, same source
/// `vc status` reads. Zero hits is success (exit 0), not an error: an
/// agent needs to tell "found nothing" apart from "the command failed."
fn cmd_query(
    root: &Path,
    cwd: &Path,
    pattern: &str,
    regex: bool,
    budget: Option<usize>,
    paths: &[std::path::PathBuf],
) -> VcResult<CmdOutcome> {
    let scope = paths
        .iter()
        .map(|p| rebase_user_path(root, cwd, p))
        .collect::<VcResult<Vec<_>>>()?;

    let (_ix, epoch) = index::refresh(root)?;
    let epoch8 = index::epoch8(&epoch).to_string();

    let hits = if regex {
        velocity_code_query::search_regex(root, pattern, &scope)?
    } else {
        velocity_code_query::search_literal(root, pattern, &scope)?
    };
    let n = hits.len();
    let budgeted = velocity_code_query::render_hits(&hits, budget);

    let mut human = format!("epoch {epoch8} — {n} hits\n");
    if !budgeted.text.is_empty() {
        human.push_str(&budgeted.text);
        human.push('\n');
    }
    if budgeted.elided > 0 {
        human.push_str(&format!("… elided {} hits (budget)\n", budgeted.elided));
    }

    // `render_hits` walks `hits` front-to-back, greedily including whole
    // hits until the running token estimate would exceed `budget`, so the
    // hits it actually rendered into `budgeted.text` are exactly the first
    // `hits.len() - budgeted.elided` of them — slicing here keeps the
    // `--json` `hits` array consistent with `elided` (their counts must
    // sum to the total match count) instead of dumping every match
    // regardless of budget.
    let included = n - budgeted.elided;
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

    let files = hits
        .iter()
        .map(|h| &h.path)
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    Ok(CmdOutcome {
        human,
        json,
        files,
        edits: n,
        epoch8,
        warning: None,
    })
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

    let (files, edits, epoch8, refusal) = match &result {
        Ok(o) => (o.files, o.edits, o.epoch8.clone(), None),
        Err(e) => (0, 0, String::new(), Some(output::error_kind_label(e.kind))),
    };
    metrics::record(&repo_root, verb, ms, files, edits, refusal, &epoch8);

    let code = output::emit(cli.json, &result);
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
