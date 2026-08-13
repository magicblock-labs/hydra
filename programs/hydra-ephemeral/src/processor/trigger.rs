//! `Trigger` (disc 1).
//!
//! Runs a crank's scheduled ixs once, on the ephemeral rollup. Same top-level +
//! instructions-sysvar + memcmp follow-up verification, cranker payout and
//! schedule advance as base `Trigger`. The crank holds its reward budget as a
//! plain lamport balance (funded by the sponsor); `rent_min` is `0` because the
//! ephemeral account's rent lives in the Magic vault, not in the account.
//!
//! Accounts: `[crank(w), cranker(w,s), instructions_sysvar]`.

use pinocchio::{
    error::ProgramError, sysvars::instructions::INSTRUCTIONS_ID, AccountView, ProgramResult,
};

use hydra_api::{
    consts::{ephemeral, REMAINING_INFINITE},
    program::{
        helpers::{get_clock_slot, get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT},
        processor::verify_followup,
    },
    state::{load_crank, load_crank_mut},
    HydraError,
};

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [crank_ai, cranker_ai, ix_sysvar_ai] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    if !cranker_ai.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !crank_ai.owned_by(&crate::ID) {
        return Err(ProgramError::InvalidAccountOwner);
    }
    if ix_sysvar_ai.address() != &INSTRUCTIONS_ID {
        return Err(ProgramError::UnsupportedSysvar);
    }
    if get_stack_height() != TRANSACTION_LEVEL_STACK_HEIGHT {
        return Err(HydraError::InvalidInstruction.into());
    }

    let current_slot = get_clock_slot()?;

    let (next_exec_slot, interval_slots, remaining, priority_tip, executed, region_len) = {
        let data = crank_ai.try_borrow()?;
        let s = unsafe { load_crank(&data)? };
        (
            s.next_exec_slot(),
            s.interval_slots(),
            s.remaining(),
            s.priority_tip(),
            s.executed(),
            s.region_len(),
        )
    };

    if current_slot < next_exec_slot {
        return Err(HydraError::NotYetExecutable.into());
    }
    if remaining == 0 {
        return Err(HydraError::Exhausted.into());
    }

    // Cranker reward, paid out of the crank's balance — same as base `Trigger`,
    // but at the ephemeral-rollup rate.
    let reward = ephemeral::CRANKER_REWARD
        .checked_add(priority_tip)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let new_crank_lamports = crank_ai
        .lamports()
        .checked_sub(reward)
        .ok_or::<ProgramError>(HydraError::InsufficientFunds.into())?;

    verify_followup(ix_sysvar_ai, crank_ai, region_len as usize)?;

    // Pay the cranker via direct lamport mutation.
    crank_ai.set_lamports(new_crank_lamports);
    let new_cranker_lamports = cranker_ai
        .lamports()
        .checked_add(reward)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    cranker_ai.set_lamports(new_cranker_lamports);

    let next_slot = next_exec_slot
        .checked_add(interval_slots)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    {
        let mut data = crank_ai.try_borrow_mut()?;
        let s = unsafe { load_crank_mut(&mut data)? };
        s.set_next_exec_slot(next_slot);
        s.set_executed(executed + 1);
        if remaining != REMAINING_INFINITE {
            s.set_remaining(remaining - 1);
        }
    }

    Ok(())
}
