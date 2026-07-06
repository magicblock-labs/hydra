//! `Close` (disc 3).
//!
//! Permissionless cleanup of an exhausted / underfunded / stuck ephemeral crank.
//! Pays the cranker bounty to `reporter` and refunds the remaining balance to
//! `recipient` (shared with base via `process_close`), then CPIs Magic
//! `CloseEphemeralAccount` to deallocate the account and refund its vault rent
//! to `reporter`. If the crank has a non-zero authority, only that authority may
//! be the refund `recipient` (anti-grief, enforced inside `process_close`).
//!
//! Accounts: `[reporter(w,s), crank(w), recipient(w), vault(w), magic_program(ro)]`.

use ephemeral_rollups_pinocchio::ephemeral_accounts::EphemeralAccount;
use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::program::processor::process_close;

use crate::processor::common::check_magic_accounts;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [reporter, crank_ai, recipient, vault, magic_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    check_magic_accounts(vault, magic_program)?;

    // Base-identical payout: closable check + bounty/refund split, zeroes crank.
    process_close(reporter, crank_ai, recipient, &crate::ID, false)?;

    // Deallocate the now zero-lamport ephemeral account; Magic refunds the vault
    // rent to `reporter`.
    EphemeralAccount::new(reporter, crank_ai, vault, magic_program).close()
}
