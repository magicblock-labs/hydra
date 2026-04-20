//! Minimal Pinocchio integrator.
//!
//! `schedule` builds a Hydra `Create` payload on the stack.

#![no_std]

use pinocchio::{
    cpi::invoke,
    error::ProgramError,
    instruction::{InstructionAccount, InstructionView},
    no_allocator, nostd_panic_handler, program_entrypoint, AccountView, Address, ProgramResult,
};

use hydra_api::{
    consts::{ix as disc, MAX_ACCOUNTS, MAX_DATA_LEN},
    instruction::CREATE_FIXED_PREFIX_LEN,
};

program_entrypoint!(process);
no_allocator!();
nostd_panic_handler!();

const DISC_SCHEDULE: u8 = 0;

/// Stack-allocated buffer big enough for any Hydra `Create` payload:
/// `1 byte disc + CREATE_FIXED_PREFIX_LEN + 33*MAX_ACCOUNTS + MAX_DATA_LEN`.
const CREATE_BUF_MAX: usize = 1 + CREATE_FIXED_PREFIX_LEN + 33 * MAX_ACCOUNTS + MAX_DATA_LEN;

pub fn process(_program_id: &Address, accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
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
    let seed = &data[0..32];
    let target_program_id = &data[32..64];
    let tick_len = u16::from_le_bytes([data[64], data[65]]) as usize;
    if data.len() < 66 + tick_len || tick_len > MAX_DATA_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    let tick_data = &data[66..66 + tick_len];

    let [payer, crank, system_program, hydra_program, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // Build Hydra Create data on the stack.
    let mut buf = [0u8; CREATE_BUF_MAX];
    let mut cursor = 0usize;
    buf[cursor] = disc::CREATE;
    cursor += 1;
    buf[cursor..cursor + 32].copy_from_slice(seed);
    cursor += 32;
    buf[cursor..cursor + 32].copy_from_slice(&[0u8; 32]); // no cancel authority
    cursor += 32;
    buf[cursor..cursor + 8].copy_from_slice(&0u64.to_le_bytes()); // start_slot
    cursor += 8;
    buf[cursor..cursor + 8].copy_from_slice(&400u64.to_le_bytes()); // interval_slots
    cursor += 8;
    buf[cursor..cursor + 8].copy_from_slice(&10u64.to_le_bytes()); // remaining
    cursor += 8;
    buf[cursor..cursor + 8].copy_from_slice(&1_000u64.to_le_bytes()); // priority_tip
    cursor += 8;
    buf[cursor..cursor + 4].copy_from_slice(&0u32.to_le_bytes()); // cu_limit (omit)
    cursor += 4;
    buf[cursor] = 0; // num_accounts
    cursor += 1;
    buf[cursor..cursor + 2].copy_from_slice(&(tick_len as u16).to_le_bytes());
    cursor += 2;
    buf[cursor..cursor + 32].copy_from_slice(target_program_id);
    cursor += 32;
    // No metas.
    buf[cursor..cursor + tick_len].copy_from_slice(tick_data);
    cursor += tick_len;

    let metas = [
        InstructionAccount::writable_signer(payer.address()),
        InstructionAccount::writable(crank.address()),
        InstructionAccount::readonly(system_program.address()),
    ];
    let ix = InstructionView {
        program_id: hydra_program.address(),
        accounts: &metas,
        data: &buf[..cursor],
    };
    invoke(&ix, &[payer, crank, system_program])
}
