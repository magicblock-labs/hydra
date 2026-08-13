//! `Cancel` (disc 2).
//!
//! Authority-gated drain of the crank's lamport balance to `recipient` (shared
//! with base via `process_cancel`), then a Magic `CloseEphemeralAccount` CPI to
//! deallocate the ephemeral account and refund its vault rent to `authority`.
//!
//! Accounts: `[authority(w,s), crank(w), recipient(w), vault(w), magic_program(ro)]`.

use ephemeral_rollups_pinocchio::ephemeral_accounts::EphemeralAccount;
use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::program::processor::process_cancel;

use crate::processor::common::check_magic_accounts;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [authority, crank_ai, recipient, vault, magic_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    check_magic_accounts(vault, magic_program)?;

    process_cancel(authority, crank_ai, recipient, &crate::ID)?;

    // Deallocate the now zero-lamport ephemeral account; Magic refunds the vault
    // rent to `authority`.
    EphemeralAccount::new(authority, crank_ai, vault, magic_program).close()
}
