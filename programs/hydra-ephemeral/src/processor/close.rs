//! `Close` (disc 3).
//!
//! Permissionless cleanup of an exhausted or stuck ephemeral crank. CPIs Magic
//! `CloseEphemeralAccount`, refunding the rent to `reporter`. If the crank has a
//! non-zero authority, only that authority may close it (so the refund can't be
//! redirected away from them).
//!
//! Accounts: `[reporter(w,s), crank(w), vault(w), magic_program(ro)]`.

use ephemeral_rollups_pinocchio::ephemeral_accounts::EphemeralAccount;
use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::{state::load_crank, HydraError, STALENESS_THRESHOLD_SLOTS};

use hydra_api::program::helpers::get_clock_slot;
use hydra_api::program::processor::{require_refund_recipient, require_signed_crank};

use crate::processor::common::check_magic_accounts;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [reporter, crank_ai, vault, magic_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    require_signed_crank(reporter, crank_ai, &crate::ID)?;
    check_magic_accounts(vault, magic_program)?;

    let (stored_authority, remaining, next_exec_slot) = {
        let data = crank_ai.try_borrow()?;
        let s = unsafe { load_crank(&data)? };
        (s.authority, s.remaining(), s.next_exec_slot())
    };

    // Exhausted OR stuck. There is no "underfunded" case: an ephemeral crank
    // never holds lamports.
    let exhausted = remaining == 0;
    let current_slot = get_clock_slot()?;
    let stuck = current_slot.saturating_sub(next_exec_slot) > STALENESS_THRESHOLD_SLOTS;
    if !(exhausted || stuck) {
        return Err(HydraError::NotClosable.into());
    }

    // The refund goes to `reporter`, so anti-grief binds the closer to the authority.
    require_refund_recipient(stored_authority, reporter)?;

    EphemeralAccount::new(reporter, crank_ai, vault, magic_program).close()
}
