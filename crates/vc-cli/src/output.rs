//! Human/JSON presentation and the human<->stdout, json<->stdout,
//! error<->stderr-or-stdout routing that the spec §2.3 ritual pins.
//!
//! Every command handler in `main.rs` builds a [`CmdOutcome`] on success
//! (both a pre-rendered human string and a pre-built `serde_json::Value`,
//! since the exact shape of each differs per verb and is cheaper to build
//! once at the call site than to reverse-engineer generically here).
//! [`emit`] is the single place that decides which of the two reaches
//! stdout, and where a `VcError` goes on failure — the contract is
//! asymmetric: human errors print to stderr, `--json` errors print the
//! `{"error": ...}` envelope to *stdout* (so a machine caller reads one
//! stream), and both still exit with `VcError::exit_code()`.

use velocity_code_kernel::{ErrorKind, VcError};

/// A finished command result: the human text to print (already
/// newline-terminated by its builder) and the equivalent JSON value, plus
/// the fields `main.rs` needs to fill in this invocation's metrics line.
pub struct CmdOutcome {
    pub human: String,
    pub json: serde_json::Value,
    pub files: usize,
    pub edits: usize,
    pub epoch8: String,
    /// Set only for `apply`/`undo` when the mutation itself fully
    /// succeeded but the post-commit index refresh degraded
    /// (`ApplyReport.warning`). Printed to stderr even on the success
    /// path — it is not a failure, just a "run `vc status`" heads-up.
    pub warning: Option<String>,
}

/// Mirrors `velocity_code_kernel::errors::ErrorKind`'s private `label()`
/// mapping (exhaustively matched, so a new kernel `ErrorKind` variant is a
/// compile error here, not a silent gap). The CLI's `--json` contract
/// needs `kind` as a standalone machine string distinct from `VcError`'s
/// `Display` grammar, and `label()` isn't `pub` — duplicating the ten
/// arms was judged lower-risk than reopening an already-reviewed kernel
/// module for this task; see the task report for the full tradeoff.
pub fn error_kind_label(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Usage => "usage",
        ErrorKind::Stale => "stale",
        ErrorKind::ScopeDrift => "scope-drift",
        ErrorKind::JournalBlocked => "journal-blocked",
        ErrorKind::NotFound => "not-found",
        ErrorKind::Ambiguous => "ambiguous",
        ErrorKind::Overlap => "overlap",
        ErrorKind::Malformed => "malformed",
        ErrorKind::Toctou => "toctou",
        ErrorKind::Io => "io",
        ErrorKind::Budget => "budget",
    }
}

fn error_json(e: &VcError) -> serde_json::Value {
    serde_json::json!({
        "error": {
            "kind": error_kind_label(e.kind),
            "message": &e.message,
            "next": &e.next,
        }
    })
}

/// Print `result` per the output contract and return the process exit
/// code: 0 on success, `VcError::exit_code()` on failure. Never panics on
/// I/O — a broken pipe on stdout/stderr just means the writes are no-ops.
pub fn emit(json_mode: bool, result: &Result<CmdOutcome, VcError>) -> i32 {
    match result {
        Ok(o) => {
            if let Some(w) = &o.warning {
                eprintln!("warning: {w}");
            }
            if json_mode {
                println!("{}", o.json);
            } else {
                print!("{}", o.human);
            }
            0
        }
        Err(e) => {
            if json_mode {
                println!("{}", error_json(e));
            } else {
                eprintln!("{e}");
            }
            e.exit_code()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_label_matches_error_grammar_spelling() {
        // Spot-check against VcError's own Display, which uses the
        // kernel's private label() — if these ever drift, Display's
        // "<label>: <message>" prefix and this fn's output would too.
        let e = VcError::new(ErrorKind::Stale, "x");
        assert!(e.to_string().starts_with("stale:"));
        assert_eq!(error_kind_label(ErrorKind::Stale), "stale");
        assert_eq!(
            error_kind_label(ErrorKind::JournalBlocked),
            "journal-blocked"
        );
        assert_eq!(error_kind_label(ErrorKind::ScopeDrift), "scope-drift");
        assert_eq!(error_kind_label(ErrorKind::NotFound), "not-found");
        assert_eq!(error_kind_label(ErrorKind::Budget), "budget");
    }

    #[test]
    fn json_error_envelope_carries_kind_message_next() {
        let e = VcError::new(ErrorKind::Stale, "boom").with_next("vc status");
        let v = error_json(&e);
        assert_eq!(v["error"]["kind"], "stale");
        assert_eq!(v["error"]["message"], "boom");
        assert_eq!(v["error"]["next"], "vc status");
    }

    #[test]
    fn json_error_envelope_next_is_null_when_absent() {
        let e = VcError::new(ErrorKind::Io, "boom");
        let v = error_json(&e);
        assert!(v["error"]["next"].is_null());
    }

    #[test]
    fn emit_success_json_prints_json_not_human() {
        // emit() itself writes to real stdout/stderr (it's a thin CLI
        // wrapper), so this just pins the exit code contract: 0 on Ok.
        let outcome = CmdOutcome {
            human: "human\n".into(),
            json: serde_json::json!({"a": 1}),
            files: 0,
            edits: 0,
            epoch8: String::new(),
            warning: None,
        };
        assert_eq!(emit(true, &Ok(outcome)), 0);
    }

    #[test]
    fn emit_error_returns_mapped_exit_code() {
        let err = VcError::new(ErrorKind::Stale, "boom");
        assert_eq!(emit(false, &Err(err)), 3);
        let err = VcError::new(ErrorKind::Usage, "boom");
        assert_eq!(emit(true, &Err(err)), 2);
    }
}
