//! `Cancel` (disc 2) — authority-gated close + rent refund.

use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use crate::processor::common::drain_lamports;
use hydra_api::program::processor::require_cancel_authority;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [authority, crank_ai, recipient] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    require_cancel_authority(authority, crank_ai, &crate::ID)?;
    drain_lamports(crank_ai, recipient)
}
