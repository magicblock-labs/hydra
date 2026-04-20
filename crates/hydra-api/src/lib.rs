//! Hydra — shared types and constants for the on-chain program and its clients.
//!
//! The program itself is `no_std`; enable the `client` feature when consuming
//! this crate from a host-side binary that wants the instruction builders.

#![cfg_attr(not(feature = "client"), no_std)]

pub mod consts;
#[cfg(any(feature = "cpi-native", feature = "cpi-pinocchio"))]
pub mod cpi;
pub mod error;
pub mod instruction;
pub mod state;

pub use consts::*;
pub use error::HydraError;
pub use state::Crank;

use solana_address::declare_id;

declare_id!("Hydra17i1feui9deaxu6d1TzSQMRNHeBRkDR1Awy7zea");
