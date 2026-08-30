//! velocity-code select

pub mod diff;
pub use diff::{edits_from_args, edits_from_diff};

pub mod matcher;
pub use matcher::{ContentByPath, MatchSite, match_sites};
