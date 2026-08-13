//! Base-layer crank lifecycle.
//!
//! Schedule parsing, tail serialization and follow-up verification are shared
//! with the ephemeral-rollup program via [`hydra_api::program`]; only the
//! System-program funding model lives here.

pub mod cancel;
pub mod close;
pub mod create;
pub mod trigger;

mod common;
