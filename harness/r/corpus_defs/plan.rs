//! Plan domain model — mirrors the shapes used by vc's own planning layer,
//! kept intentionally decorative for the R1 definitions ground truth corpus.
//! Frozen fixture: do not "clean up" unused fields/variants here.

use std::fmt;

pub const DEFAULT_BUDGET: usize = 4096;

pub static PLAN_VERSION: u32 = 3;

pub type PlanId = String;

#[derive(Debug, Clone)]
pub struct Plan {
    pub id: PlanId,
    pub version: u32,
    pub sites: Vec<Site>,
}

#[derive(Debug, Clone)]
pub struct Site {
    pub path: String,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanKind {
    Edit,
    Delete,
    Rename,
}

impl Plan {
    pub fn new(id: PlanId) -> Self {
        Plan {
            id,
            version: PLAN_VERSION,
            sites: Vec::new(),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn add_site(&mut self, site: Site) {
        self.sites.push(site);
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plan {} ({} sites)", self.id, self.sites.len())
    }
}

pub trait Summarize {
    fn summary(&self) -> String;

    fn short_summary(&self) -> String {
        self.summary()
    }
}

impl Summarize for Plan {
    fn summary(&self) -> String {
        format!("{} v{}", self.id, self.version)
    }
}

pub fn merge_plans<T: Clone>(items: &[T]) -> Vec<T> {
    items.to_vec()
}

pub fn largest_site(sites: &[Site]) -> Option<&Site> {
    sites.iter().max_by_key(|s| s.line)
}

pub mod budget {
    pub const MAX_TOKENS: usize = 8000;

    pub fn within(tokens: usize) -> bool {
        tokens <= MAX_TOKENS
    }
}
