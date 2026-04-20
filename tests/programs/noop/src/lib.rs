//! Minimal no-op program used as a scheduled-ix target in Hydra's CU tests.
//!
//! Consumes only the entrypoint + `Ok(())` overhead (~20 CU), so when the
//! `cu_table` test reports `tx CU`, essentially all of it comes from Hydra
//! itself instead of being dominated by SPL Memo's ~2.8 k CU.

#![no_std]

use pinocchio::{
    no_allocator, nostd_panic_handler, program_entrypoint, AccountView, Address, ProgramResult,
};

program_entrypoint!(process);
no_allocator!();
nostd_panic_handler!();

pub fn process(
    _program_id: &Address,
    _accounts: &mut [AccountView],
    _data: &[u8],
) -> ProgramResult {
    Ok(())
}
