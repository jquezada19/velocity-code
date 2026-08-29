//! velocity-code kernel

pub mod errors;
pub use errors::{ErrorKind, VcError, VcResult};

pub mod apply;
pub mod fault;
pub mod hash;
pub mod index;
pub mod journal;
pub mod lock;
pub mod plan;
pub mod resolve;
pub mod root;
pub mod walk;
