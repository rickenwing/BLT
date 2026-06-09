//! Shared app plumbing used by both binaries (feature `runtime`): data-root
//! resolution and rotating-file logging. Kept out of the default build so the
//! pure-logic core stays dependency-light and its tests stay fast.

pub mod data_root;
pub mod logging;

pub use data_root::{Component, ComponentDirs, DataRoot};
