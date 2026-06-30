//! `Close` (disc 3) — permissionless cleanup of exhausted / underfunded /
//! stuck cranks. A crank is "stuck" when `next_exec_slot` has fallen more
//! than `STALENESS_THRESHOLD_SLOTS` behind the current slot, which means no
//! cranker has successfully fired it in ~10 days — almost always because the
//! inner ix deterministically fails.
//!
//! The closable check + bounty/refund payout is shared with the ephemeral
//! program via [`hydra_api::program::processor::process_close`].

use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::program::processor::process_close;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [reporter, crank_ai, recipient] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    process_close(reporter, crank_ai, recipient, &crate::ID)
}
