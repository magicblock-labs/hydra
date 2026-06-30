//! `Create` (disc 0).
//!
//! Allocates the crank as a MagicBlock ephemeral account (owned by Hydra) via a
//! Magic-program CPI — which materializes it synchronously — then writes the
//! `Crank` header + scheduled-ix tail, all in one instruction.
//!
//! Accounts: `[sponsor(w,s), crank(w), vault(w), magic_program(ro)]`.
//! Data: identical to base `Create`'s body (seed, authority, schedule, scheduled
//! ixs). `sponsor` sets `authority_signer` and pays the per-byte ephemeral rent.

use ephemeral_rollups_pinocchio::ephemeral_accounts::EphemeralAccount;
use pinocchio::{
    cpi::{Seed, Signer},
    error::ProgramError,
    AccountView, ProgramResult,
};

use hydra_api::consts::{CRANK_HEADER_SIZE, CRANK_SEED_PREFIX};
use hydra_api::program::processor::{
    derive_crank_pda, measure_region, parse_create_header, write_crank,
};

use crate::processor::common::check_magic_accounts;

pub fn process(accounts: &[AccountView], data: &[u8]) -> ProgramResult {
    let [sponsor, crank_ai, vault, magic_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    let header = parse_create_header(data)?;
    let authority_signer: u8 =
        (sponsor.address().as_array() == &header.authority && sponsor.is_signer()) as u8;

    check_magic_accounts(vault, magic_program)?;

    // Size the account from the scheduled ixs (validates the schedule), then
    // allocate it. The exact tail is written below.
    let region_len = measure_region(data)?;
    let bump = derive_crank_pda(crank_ai, &header.seed, &crate::ID)?;
    let data_len = CRANK_HEADER_SIZE + region_len;

    // The crank PDA must sign the create CPI (the ephemeral account is a signer
    // on create, to prevent pubkey squatting); Hydra signs it with the seeds.
    let bump_arr = [bump];
    let seeds = [
        Seed::from(CRANK_SEED_PREFIX),
        Seed::from(header.seed.as_ref()),
        Seed::from(&bump_arr),
    ];
    let signer = Signer::from(&seeds);

    EphemeralAccount::new(sponsor, crank_ai, vault, magic_program)
        .with_signers(&[signer])
        .create(data_len as u32)?;

    // The account is sized to `region_len`, so `write_crank` fills it exactly.
    // Ephemeral cranks hold no lamports, so the rent floor is `0`.
    write_crank(
        crank_ai,
        data,
        &header,
        bump,
        authority_signer,
        0,
        region_len,
    )
}
