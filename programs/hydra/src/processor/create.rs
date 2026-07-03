//! `Create` (disc 0).
//!
//! Wire layout for ix data (no alignment padding):
//!
//! ```text
//! seed:             [u8; 32]
//! authority:        [u8; 32]
//! start_slot:       u64 LE
//! interval_slots:   u64 LE
//! remaining:        u64 LE   // 0 = infinite (stored internally as u64::MAX)
//! priority_tip:     u64 LE
//! cu_limit:         u32 LE   // 0 = cranker omits SetComputeUnitLimit
//! ── one or more scheduled ixs, parsed until the data is exhausted: ──
//!   num_accounts:   u8
//!   data_len:       u16 LE
//!   program_id:     [u8; 32]
//!   metas:          [[flag:u8][pubkey:[u8;32]]; num_accounts]
//!   data:           [u8; data_len]
//! ```

use pinocchio::{
    cpi::{Seed, Signer},
    error::ProgramError,
    sysvars::{rent::Rent, Sysvar},
    AccountView, ProgramResult,
};
#[cfg(not(feature = "create-account-allow-prefund"))]
use pinocchio_system::instructions::{Allocate, Assign, CreateAccount, Transfer};
#[cfg(feature = "create-account-allow-prefund")]
use pinocchio_system::instructions::{CreateAccountAllowPrefund, Funding};

use hydra_api::{
    consts::{CRANK_HEADER_SIZE, CRANK_SEED_PREFIX},
    program::processor::{derive_crank_pda, measure_region, parse_create_header, write_crank},
};

pub fn process(accounts: &[AccountView], data: &[u8]) -> ProgramResult {
    let [payer, crank_ai, _system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    let header = parse_create_header(data)?;
    let authority_signer: u8 = (payer.address().as_array() == &header.authority) as u8;

    // Size the account from the scheduled ixs (validates the schedule), verify
    // the PDA, then allocate at exactly that size. The tail is written below.
    let region_len = measure_region(data)?;
    let bump = derive_crank_pda(crank_ai, &header.seed, &crate::ID)?;
    let total_size = CRANK_HEADER_SIZE + region_len;

    // One sysvar read serves both CreateAccount funding and the cached floor.
    let rent = Rent::get()?;
    let rent_min = rent.try_minimum_balance(total_size)?;

    // Sign the CreateAccount with the PDA's seeds so it owns itself on creation.
    let bump_arr = [bump];
    let seeds_arr = [
        Seed::from(CRANK_SEED_PREFIX),
        Seed::from(header.seed.as_ref()),
        Seed::from(&bump_arr),
    ];
    let signers = [Signer::from(&seeds_arr)];

    #[cfg(feature = "create-account-allow-prefund")]
    {
        let funding_lamports = rent_min.saturating_sub(crank_ai.lamports());
        if funding_lamports == 0 && !payer.is_signer() {
            return Err(ProgramError::MissingRequiredSignature);
        }
        CreateAccountAllowPrefund {
            to: crank_ai,
            space: total_size as u64,
            owner: &crate::ID,
            funding: (funding_lamports > 0).then_some(Funding {
                from: payer,
                lamports: funding_lamports,
            }),
        }
        .invoke_signed(&signers)?;
    }

    #[cfg(not(feature = "create-account-allow-prefund"))]
    {
        let prefunded = crank_ai.lamports();
        if prefunded == 0 {
            // Fresh PDA (the common case): one `CreateAccount` CPI funds,
            // allocates, and assigns in a single system-program invocation —
            // a third of the CU of the split path below.
            CreateAccount {
                from: payer,
                to: crank_ai,
                lamports: rent_min,
                space: total_size as u64,
                owner: &crate::ID,
            }
            .invoke_signed(&signers)?;
        } else {
            // Prefunded PDA: `CreateAccount` rejects accounts that already hold
            // lamports, so top up the shortfall then allocate + assign.
            let funding_lamports = rent_min.saturating_sub(prefunded);
            if funding_lamports == 0 && !payer.is_signer() {
                return Err(ProgramError::MissingRequiredSignature);
            }
            if funding_lamports > 0 {
                Transfer {
                    from: payer,
                    to: crank_ai,
                    lamports: funding_lamports,
                }
                .invoke()?;
            }
            Allocate {
                account: crank_ai,
                space: total_size as u64,
            }
            .invoke_signed(&signers)?;
            Assign {
                account: crank_ai,
                owner: &crate::ID,
            }
            .invoke_signed(&signers)?;
        }
    }

    // The account is sized to `region_len`, so `write_crank` fills it exactly.
    write_crank(
        crank_ai,
        data,
        &header,
        bump,
        authority_signer,
        rent_min,
        region_len,
    )
}
