//! `Cancel` (disc 2) — authority-gated close + rent refund.

use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::{state::load_crank, HydraError};

pub fn process(accounts: &mut [AccountView], _data: &[u8]) -> ProgramResult {
    let [authority, crank_ai, recipient] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    if !authority.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !crank_ai.owned_by(&hydra_api::ID) {
        return Err(ProgramError::InvalidAccountOwner);
    }

    // Read the stored authority and bail if unkillable (all-zeros) or
    // doesn't match the signer.
    let stored_authority = {
        let data = crank_ai.try_borrow()?;
        let state = unsafe { load_crank(&data)? };
        state.authority
    };

    if stored_authority == [0u8; 32] {
        return Err(HydraError::UnauthorizedAuthority.into());
    }
    if authority.address().as_array() != &stored_authority {
        return Err(HydraError::UnauthorizedAuthority.into());
    }

    drain_lamports(crank_ai, recipient)
}

/// Move all lamports out of `src` into `dst`. The runtime zeroes `src.data`
/// and reassigns ownership to the system program at the instruction boundary
/// because `src.lamports == 0` post-write.
#[inline(always)]
pub(super) fn drain_lamports(src: &mut AccountView, dst: &mut AccountView) -> ProgramResult {
    let amount = src.lamports();
    let new_dst = dst
        .lamports()
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    src.set_lamports(0);
    dst.set_lamports(new_dst);
    Ok(())
}
