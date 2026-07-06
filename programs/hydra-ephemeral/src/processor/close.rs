//! `Close` (disc 3).
//!
//! Cleanup of an exhausted / underfunded / stuck ephemeral crank. Pays the
//! cranker bounty to `reporter` and refunds the remaining balance to `recipient`
//! (shared with base via `process_close`), then CPIs Magic
//! `CloseEphemeralAccount` to deallocate the account and refund its vault rent.
//!
//! Unlike the base `Close`, this is only permissionless for *unowned* cranks
//! (`authority == 0`). When a crank carries a non-zero authority, only that
//! authority may close it. The reason is the vault rent: Magic refunds it to the
//! teardown's signer, so a permissionless close would hand an owned crank's rent
//! to an arbitrary `reporter`. Gating owned cranks to their authority keeps the
//! whole teardown — bounty, leftover balance, and vault rent — with the owner,
//! while unowned cranks stay permissionlessly closable by anyone.
//!
//! Accounts: `[reporter(w,s), crank(w), recipient(w), vault(w), magic_program(ro)]`.

use ephemeral_rollups_pinocchio::ephemeral_accounts::EphemeralAccount;
use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::{program::processor::process_close, state::load_crank, HydraError};

use crate::processor::common::check_magic_accounts;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [reporter, crank_ai, recipient, vault, magic_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    check_magic_accounts(vault, magic_program)?;

    // A crank with an authority can only be closed by that authority: Magic
    // refunds the vault rent to the teardown's signer, so this keeps an owned
    // crank's rent with its owner instead of an arbitrary reporter. Unowned
    // cranks (`authority == 0`) stay permissionlessly closable.
    let stored_authority = {
        let data = crank_ai.try_borrow()?;
        unsafe { load_crank(&data)? }.authority
    };
    if stored_authority != [0u8; 32] && reporter.address().as_array() != &stored_authority {
        return Err(HydraError::UnauthorizedAuthority.into());
    }

    // Closable check + bounty/refund split, zeroes crank (ephemeral economics).
    process_close(reporter, crank_ai, recipient, &crate::ID, true)?;

    // Deallocate the now zero-lamport ephemeral account; Magic refunds the vault
    // rent to `reporter` (== the authority for owned cranks, per the gate above).
    EphemeralAccount::new(reporter, crank_ai, vault, magic_program).close()
}
