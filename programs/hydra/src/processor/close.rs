//! `Close` (disc 3) — permissionless cleanup of exhausted / underfunded /
//! stuck cranks. A crank is "stuck" when `next_exec_slot` has fallen more
//! than `STALENESS_THRESHOLD_SLOTS` behind the current slot, which means no
//! cranker has successfully fired it in ~31 days — almost always because the
//! inner ix deterministically fails.

use pinocchio::{error::ProgramError, AccountView, ProgramResult};

use hydra_api::{
    consts::{CRANKER_REWARD, STALENESS_THRESHOLD_SLOTS},
    state::load_crank,
    HydraError,
};

use crate::helpers::get_clock_slot;

pub fn process(accounts: &mut [AccountView], _data: &[u8]) -> ProgramResult {
    let [reporter, crank_ai, recipient] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    if !reporter.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !crank_ai.owned_by(&hydra_api::ID) {
        return Err(ProgramError::InvalidAccountOwner);
    }

    // Snapshot fields we need from the crank header.
    let (stored_authority, remaining, rent_min, priority_tip, next_exec_slot, lamports_now) = {
        let data = crank_ai.try_borrow()?;
        let state = unsafe { load_crank(&data)? };
        (
            state.authority,
            state.remaining(),
            state.rent_min(),
            state.priority_tip(),
            state.next_exec_slot(),
            crank_ai.lamports(),
        )
    };

    // Pre-condition: exhausted OR underfunded OR stuck.
    let exhausted = remaining == 0;
    let next_reward = CRANKER_REWARD
        .checked_add(priority_tip)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let underfunded = lamports_now
        < rent_min
            .checked_add(next_reward)
            .ok_or(ProgramError::ArithmeticOverflow)?;
    // `next_exec_slot` only advances on *successful* `Trigger`, so persistent
    // failure pins it in the past. `saturating_sub` makes future-scheduled
    // cranks (`next_exec_slot > current_slot`) trivially not-stale.
    let current_slot = get_clock_slot()?;
    let stuck = current_slot.saturating_sub(next_exec_slot) > STALENESS_THRESHOLD_SLOTS;

    if !(exhausted || underfunded || stuck) {
        return Err(HydraError::NotClosable.into());
    }

    // Anti-grief: if an authority is set, only they can receive the refund.
    // Anyone can still invoke Close, but they can't redirect the rent refund.
    if stored_authority != [0u8; 32] && recipient.address().as_array() != &stored_authority {
        return Err(HydraError::UnauthorizedAuthority.into());
    }

    // Flat bounty (2 × base fee) to whoever cranked the cleanup; the balance
    // refunds to `recipient`. `min` handles a crank holding less than the
    // bounty — reporter gets what's there, recipient gets nothing.
    let bounty = CRANKER_REWARD.min(lamports_now);
    let refund = lamports_now - bounty;

    crank_ai.set_lamports(0);

    let new_reporter = reporter
        .lamports()
        .checked_add(bounty)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    reporter.set_lamports(new_reporter);

    // When `recipient` aliases `reporter`, the write above is visible here, so
    // adding `refund` on top preserves the sum. Distinct accounts: clean credit.
    let new_recipient = recipient
        .lamports()
        .checked_add(refund)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    recipient.set_lamports(new_recipient);

    Ok(())
}
