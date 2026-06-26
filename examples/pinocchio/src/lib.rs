//! Minimal Pinocchio integrator.
//!
//! `schedule` builds a Hydra `Create` payload on the stack.

#![no_std]

use pinocchio::{
    error::ProgramError, no_allocator, nostd_panic_handler, program_entrypoint, AccountView,
    Address, ProgramResult,
};

use hydra_api::{
    consts::{MAX_ACCOUNTS, MAX_DATA_LEN},
    instruction::{CreateArgs, ScheduledIx, CREATE_FIXED_PREFIX_LEN, CREATE_IX_HEADER_LEN},
};

program_entrypoint!(process);
no_allocator!();
nostd_panic_handler!();

const DISC_SCHEDULE: u8 = 0;

/// Stack-allocated buffer big enough for a single-scheduled-ix Hydra `Create`
/// payload: `disc + sched prefix + one ix blob`.
const CREATE_BUF_MAX: usize =
    1 + CREATE_FIXED_PREFIX_LEN + CREATE_IX_HEADER_LEN + 33 * MAX_ACCOUNTS + MAX_DATA_LEN;

pub fn process(_program_id: &Address, accounts: &[AccountView], data: &[u8]) -> ProgramResult {
    let [disc_byte, rest @ ..] = data else {
        return Err(ProgramError::InvalidInstructionData);
    };
    match *disc_byte {
        DISC_SCHEDULE => schedule(accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

#[inline(never)]
fn schedule(accounts: &[AccountView], data: &[u8]) -> ProgramResult {
    // Input: [seed: 32][target_program_id: 32][tick_data: u16 LE][tick: bytes]
    if data.len() < 66 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let seed: [u8; 32] = data[0..32]
        .try_into()
        .map_err(|_| ProgramError::InvalidInstructionData)?;
    let target_program_id = &data[32..64];
    let tick_len = u16::from_le_bytes([data[64], data[65]]) as usize;
    if data.len() < 66 + tick_len || tick_len > MAX_DATA_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let tick_data = &data[66..66 + tick_len];

    let [payer, crank, system_program, _hydra_program, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // Build Hydra Create data on the stack.
    hydra_api::cpi::base::pinocchio::create::<CREATE_BUF_MAX>(
        payer,
        crank,
        system_program,
        &CreateArgs {
            seed,
            authority: [0u8; 32],
            start_slot: 0,
            interval_slots: 400,
            remaining: 10,
            priority_tip: 1_000,
            cu_limit: 0,
            scheduled: &[ScheduledIx {
                program_id: target_program_id
                    .try_into()
                    .map_err(|_| ProgramError::InvalidInstructionData)?,
                metas: &[],
                data: tick_data,
            }],
        },
        &[],
    )
}
