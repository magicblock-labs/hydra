//! On-chain building blocks shared by the base-layer and ephemeral-rollup Hydra
//! programs. Gated behind the `program` feature so client/CPI consumers don't
//! pull the pinocchio-flavoured processor code (or `solana-define-syscall`).
//!
//! Each program crate (`programs/hydra`, `programs/hydra-ephemeral`) keeps only
//! its CPI-funding-model-specific handlers; everything that is identical across
//! the two ledgers — schedule parsing, tail serialization, follow-up
//! verification, the sysvar syscalls — lives here.

pub mod helpers;
pub mod processor;
