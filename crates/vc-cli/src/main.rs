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
    plan::{Plan, PlanForm},
    recover::{self, DoctorAction},
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
    }
}

fn dispatch(root: &Path, cmd: &Cmd) -> VcResult<CmdOutcome> {
    match cmd {
        Cmd::Plan { form } => cmd_plan(root, form),
        Cmd::Show { sha8 } => cmd_show(root, sha8),
        Cmd::Apply { sha8 } => cmd_apply(root, sha8),
        Cmd::Undo { id } => cmd_undo(root, id.as_deref()),
        Cmd::Status => cmd_status(root),
        Cmd::Doctor { rollback, discard } => cmd_doctor(root, *rollback, *discard),
        Cmd::Gain { history } => cmd_gain(root, *history),
    }
}

/// `vc plan edit`/`vc plan import` -> resolve, digest, store. R1 (ledger
/// ruling): `edit` takes exactly one `--old`/`--new` pair per invocation;
/// a multi-edit plan goes through `import` in M1, repeatable `edit` pairs
/// are M2 polish.
fn cmd_plan(root: &Path, form: &PlanCmd) -> VcResult<CmdOutcome> {
    let (plan_form, reqs) = match form {
        PlanCmd::Edit { file, old, new } => {
            let reqs = edits_from_args(&[(file.clone(), old.clone(), new.clone())]);
            (PlanForm::Edit, reqs)
        }
        PlanCmd::Import => {
            let mut diff_text = String::new();
            std::io::stdin().read_to_string(&mut diff_text)?;
            let reqs = edits_from_diff(&diff_text)?;
            (PlanForm::Import, reqs)
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

fn main() {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().unwrap_or_else(|e| {
        eprintln!("io: failed to read current directory: {e}");
        std::process::exit(1);
    });
    let repo_root = root::find_root(&cwd);

    let verb = verb_name(&cli.cmd);
    let start = std::time::Instant::now();
    let result = dispatch(&repo_root, &cli.cmd);
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
