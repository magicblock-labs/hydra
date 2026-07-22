//! Ephemeral-rollup crank lifecycle.
//!
//! Schedule parsing, tail serialization and follow-up verification are shared
//! with the base-layer program via [`hydra_api::program`]; only the MagicBlock
//! ephemeral-account funding model lives here.

pub mod cancel;
pub mod close;
pub mod create;
pub mod trigger;

mod common;
