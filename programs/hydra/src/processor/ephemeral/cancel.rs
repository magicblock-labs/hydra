//! `Cancel` (disc 2).
//!
//! Authority-gated close of an ephemeral crank. CPIs Magic `CloseEphemeralAccount`,
//! refunding the rent to `authority`.
//!
//! Accounts: `[authority(w,s), crank(w), vault(w), magic_program(ro)]`.

use ephemeral_rollups_pinocchio::ephemeral_accounts::EphemeralAccount;
use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use crate::processor::common::require_cancel_authority;
use crate::processor::ephemeral::common::check_magic_accounts;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [authority, crank_ai, vault, magic_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    require_cancel_authority(authority, crank_ai, &hydra_api::ephemeral::ID)?;
    check_magic_accounts(vault, magic_program)?;

    // The ephemeral account need not sign on close; `authority` is the sponsor
    // (a real signer) and receives the rent refund.
    EphemeralAccount::new(authority, crank_ai, vault, magic_program).close()
}
