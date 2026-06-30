//! `Cancel` (disc 2) — authority-gated close + rent refund.

use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::program::processor::process_cancel;

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [authority, crank_ai, recipient] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    process_cancel(authority, crank_ai, recipient, &crate::ID)
}
