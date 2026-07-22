//! Minimal no-op program used as a scheduled-ix target in Hydra's CU tests.
//!
//! Consumes only the entrypoint + `Ok(())` overhead (~20 CU), so when the
//! `cu_table` test reports `tx CU`, essentially all of it comes from Hydra
//! itself instead of being dominated by SPL Memo's ~2.8 k CU.
//!
//! When instruction data is at least 8 bytes, the first 8 (LE `u64`) are logged
//! as `noop-fired:<id>` so live e2e tests can attribute fires via `logsSubscribe`
//! instead of polling crank account state.

#![no_std]

use pinocchio::{
    no_allocator, nostd_panic_handler, program_entrypoint, AccountView, Address, ProgramResult,
};
use pinocchio_log::logger::Logger;

program_entrypoint!(process);
no_allocator!();
nostd_panic_handler!();

pub fn process(_program_id: &Address, _accounts: &[AccountView], data: &[u8]) -> ProgramResult {
    if data.len() >= 8 {
        let id = u64::from_le_bytes(data[..8].try_into().unwrap());
        let mut logger = Logger::<32>::default();
        logger.append("noop-fired:");
        logger.append(id);
        logger.log();
    }
    Ok(())
}
