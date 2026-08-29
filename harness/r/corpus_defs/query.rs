//! Query domain shapes — hits, a ranking trait, and an intentional
//! same-name-method collision (`describe`, on two distinct types) — used
//! only to exercise the R1 definitions ground truth corpus.

use std::path::PathBuf;

pub const BINARY_SNIFF_LEN: usize = 8192;

pub static WARN_PREFIX: &str = "warning: ";

pub type Score = f64;

#[derive(Debug, Clone, PartialEq)]
pub struct QueryHit {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub struct SymbolHit {
    pub path: PathBuf,
    pub kind: HitKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Exact,
    Fuzzy,
    Elided,
}

pub trait Rankable {
    fn rank(&self) -> Score;
}

impl Rankable for QueryHit {
    fn rank(&self) -> Score {
        1.0 / (self.line as Score + 1.0)
    }
}

impl Rankable for SymbolHit {
    fn rank(&self) -> Score {
        match self.kind {
            HitKind::Exact => 1.0,
            HitKind::Fuzzy => 0.5,
            HitKind::Elided => 0.0,
        }
    }
}

impl QueryHit {
    pub fn new(path: PathBuf, line: usize, col: usize) -> Self {
        QueryHit { path, line, col }
    }

    pub fn describe(&self) -> String {
        format!("{}:{}:{}", self.path.display(), self.line, self.col)
    }
}

impl SymbolHit {
    pub fn describe(&self) -> String {
        format!("{} ({:?})", self.path.display(), self.kind)
    }
}

pub fn sort_by_rank<T: Rankable + Clone>(items: &[T]) -> Vec<T> {
    let mut out = items.to_vec();
    out.sort_by(|a, b| b.rank().partial_cmp(&a.rank()).unwrap());
    out
}

pub fn top_hit<T: Rankable>(items: &[T]) -> Option<&T> {
    items
        .iter()
        .max_by(|a, b| a.rank().partial_cmp(&b.rank()).unwrap())
}

pub mod render {
    pub const MAX_LINE_WIDTH: usize = 120;

    pub fn truncate(s: &str) -> String {
        if s.len() > MAX_LINE_WIDTH {
            s[..MAX_LINE_WIDTH].to_string()
        } else {
            s.to_string()
        }
    }
}
