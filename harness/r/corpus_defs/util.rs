//! Small utility grab-bag: a generic cache, an eviction-policy enum, and
//! decorative content for the R1 definitions negative rows plus the
//! macro-generated-looking-fn case.
//!
//! NOTE: a previous revision here called `PhantomHelper::ghost_fn()` to
//! pre-warm the cache; `PhantomHelper` never actually existed as a type in
//! this corpus and was removed. `LEGACY_TOKEN` was a static that lived here
//! before the `CacheKey` rename; `retired_widget` was a free function this
//! module used to export; `NotARealTrait` was a proposed bound this module
//! never grew. All five names below are R1 definitions negative controls —
//! they appear only in this comment, never as a real definition, so an
//! exact symbol search for each must return zero hits:
//!   - PhantomHelper (struct, never defined)
//!   - ghost_fn (method, never defined)
//!   - LEGACY_TOKEN (static, never defined)
//!   - retired_widget (function, never defined)
//!   - NotARealTrait (trait, never defined)

use std::collections::HashMap;

pub const CACHE_CAPACITY: usize = 256;

pub static EMPTY_LABEL: &str = "<empty>";

pub type CacheKey = String;

#[derive(Debug, Default)]
pub struct Cache<V> {
    entries: HashMap<CacheKey, V>,
}

impl<V> Cache<V> {
    pub fn new() -> Self {
        Cache {
            entries: HashMap::new(),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<&V> {
        self.entries.get(key)
    }

    pub fn insert(&mut self, key: CacheKey, value: V) {
        self.entries.insert(key, value);
    }
}

pub trait Evictable {
    fn should_evict(&self, age_secs: u64) -> bool;
}

pub enum EvictionPolicy {
    Lru,
    Fifo,
    Never,
}

impl Evictable for EvictionPolicy {
    fn should_evict(&self, age_secs: u64) -> bool {
        match self {
            EvictionPolicy::Lru => age_secs > 300,
            EvictionPolicy::Fifo => age_secs > 600,
            EvictionPolicy::Never => false,
        }
    }
}

pub fn clamp_capacity(requested: usize) -> usize {
    requested.min(CACHE_CAPACITY)
}

macro_rules! make_getter {
    ($name:ident, $field:ident, $ty:ty) => {
        pub fn $name(&self) -> &$ty {
            &self.$field
        }
    };
}

pub struct Widget {
    label: String,
}

impl Widget {
    // Looks like it defines `fn label(&self) -> &String { ... }` at macro
    // expansion time, but tree-sitter parses the callee as a
    // `macro_invocation` node (not `function_item`) — vc does no macro
    // expansion, so this deliberately produces NO symbol and gets no
    // ground-truth row (positive or negative).
    make_getter!(label, label, String);
}

pub mod helpers {
    pub fn identity<T>(value: T) -> T {
        value
    }
}
