//! Minimal native (`solana-program`-style) integrator.
//!
//! Exposes one instruction: given a 32-byte seed + a target program id +
//! target data, it CPIs into Hydra to create a crank that runs every 400
//! slots up to 10 times, paying a 1_000 lamport priority tip per trigger.
//!
//! Account order:
//!   0: payer            (writable, signer)
//!   1: crank            (writable) — PDA at `[b"crank", seed]` in hydra-api
//!   2: system_program
//!   3: hydra program    (implicit: pulled from hydra_api::ID inside the CPI)
//!
//! Instruction data (34 bytes):
//!   [0..32]   seed
//!   [32..64]  target_program_id
//!
//! Any signer-flagged scheduled meta is rejected at Hydra's Create.

use solana_account_info::AccountInfo;
use solana_program_entrypoint::entrypoint;
use solana_program_error::{ProgramError, ProgramResult};
use solana_pubkey::Pubkey;

use hydra_api::{cpi::native as hydra_cpi, instruction::CreateArgs};

entrypoint!(process_instruction);

pub fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let [payer, crank, system_program, _rest @ ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if data.len() < 64 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let seed: [u8; 32] = data[0..32].try_into().unwrap();
    let target_program_id = Pubkey::new_from_array(data[32..64].try_into().unwrap());

    hydra_cpi::create(
        payer,
        crank,
        system_program,
        &CreateArgs {
            seed,
            authority: [0u8; 32], // fire-and-forget; Close handles cleanup
            start_slot: 0,
            interval_slots: 400,
            remaining: 10,
            priority_tip: 1_000,
            cu_limit: 0, // no on-chain CU override
            scheduled_program_id: target_program_id,
            scheduled_metas: &[],
            scheduled_data: b"tick",
        },
    )
}
