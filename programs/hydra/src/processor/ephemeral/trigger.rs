//! `Trigger` (disc 1).
//!
//! Runs a crank's scheduled ixs once, on the ephemeral rollup. Same top-level +
//! instructions-sysvar + memcmp follow-up verification and schedule advance as
//! base `Trigger`, but moves no lamports (the ephemeral crank holds none).
//!
//! Accounts: `[crank(w), cranker(w,s), instructions_sysvar]`.

use pinocchio::{
    error::ProgramError, sysvars::instructions::INSTRUCTIONS_ID, AccountView, ProgramResult,
};

use hydra_api::{
    consts::REMAINING_INFINITE,
    state::{load_crank, load_crank_mut},
    HydraError,
};

use crate::{
    helpers::{get_clock_slot, get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT},
    processor::common::verify_followup,
};

pub fn process(accounts: &[AccountView], _data: &[u8]) -> ProgramResult {
    let [crank_ai, cranker_ai, ix_sysvar_ai] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    if !cranker_ai.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !crank_ai.owned_by(&hydra_api::ID) {
        return Err(ProgramError::InvalidAccountOwner);
    }
    if ix_sysvar_ai.address() != &INSTRUCTIONS_ID {
        return Err(ProgramError::UnsupportedSysvar);
    }
    if get_stack_height() != TRANSACTION_LEVEL_STACK_HEIGHT {
        return Err(HydraError::InvalidInstruction.into());
    }

    let current_slot = get_clock_slot()?;

    let (next_exec_slot, interval_slots, remaining, executed, region_len) = {
        let data = crank_ai.try_borrow()?;
        let s = unsafe { load_crank(&data)? };
        (
            s.next_exec_slot(),
            s.interval_slots(),
            s.remaining(),
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

    verify_followup(ix_sysvar_ai, crank_ai, region_len as usize)?;

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
