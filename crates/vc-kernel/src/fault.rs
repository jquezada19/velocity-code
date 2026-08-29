//! Deterministic crash points for testing fail-closed recovery.
//!
//! Each name below is called at a fixed structural moment inside the
//! journaled write path (`apply::apply_plan` / `apply::undo`). Under the
//! `fault-injection` feature, setting `VC_FAULT_POINT` to one of these
//! names makes the process exit immediately (code 86) the instant that
//! point is reached — no unwinding, no `Drop`, no cleanup — simulating a
//! hard crash so recovery tooling (Task 10/13) can be tested against every
//! interesting point in the sequence.
//!
//! Fixed vocabulary: "pre_journal", "post_journal_entry", "mid_files",
//! "pre_commit_marker", "post_commit_marker".
pub fn point(name: &str) {
    #[cfg(feature = "fault-injection")]
    {
        if std::env::var("VC_FAULT_POINT").as_deref() == Ok(name) {
            std::process::exit(86);
        }
    }
    let _ = name;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default `cargo test` never enables the `fault-injection` feature, so
    /// `point` must be a true no-op regardless of env state — this must
    /// never exit the test process.
    #[test]
    fn point_is_a_no_op_without_the_feature() {
        point("pre_journal");
        point("post_journal_entry");
        point("mid_files");
        point("pre_commit_marker");
        point("post_commit_marker");
        point("anything-else");
    }
}
