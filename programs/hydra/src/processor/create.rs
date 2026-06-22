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
    AccountView, Address, ProgramResult, UnsafeResize,
};
#[cfg(not(feature = "create-account-allow-prefund"))]
use pinocchio_system::instructions::{Allocate, Assign, Transfer};
#[cfg(feature = "create-account-allow-prefund")]
use pinocchio_system::instructions::{CreateAccountAllowPrefund, Funding};

use hydra_api::{
    consts::{
        ix as _ix, CRANK_HEADER_SIZE, CRANK_SEED_PREFIX, MAX_ACCOUNTS, MAX_COMPUTE_UNIT_LIMIT,
        MAX_DATA_LEN, MAX_INSTRUCTIONS, META_FLAG_SIGNER, REMAINING_INFINITE, SERIALIZED_META_SIZE,
    },
    instruction::{CREATE_FIXED_PREFIX_LEN, CREATE_IX_HEADER_LEN},
    state::load_crank_mut,
    HydraError,
};

pub fn process(accounts: &mut [AccountView], data: &[u8]) -> ProgramResult {
    let [payer, crank_ai, _system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };

    // Need at least the scheduling prefix plus one ix blob header.
    if data.len() < CREATE_FIXED_PREFIX_LEN + CREATE_IX_HEADER_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }

    // SAFETY: bounds checked above; Address/u64/u16 only require byte alignment.
    let seed: &[u8; 32] = unsafe { &*(data.as_ptr() as *const [u8; 32]) };
    let authority: &[u8; 32] = unsafe { &*(data.as_ptr().add(32) as *const [u8; 32]) };
    let start_slot = read_u64_le(data, 64);
    let interval_slots = read_u64_le(data, 72);
    let remaining_wire = read_u64_le(data, 80);
    let priority_tip = read_u64_le(data, 88);
    let cu_limit = read_u32_le(data, 96);

    // `0` is the documented opt-out. Any non-zero value must be within the
    // Solana per-tx ceiling or the runtime would reject the cranker's tx.
    if cu_limit > MAX_COMPUTE_UNIT_LIMIT {
        return Err(HydraError::InvalidSchedule.into());
    }
    // A never-advancing infinite crank makes no sense.
    if remaining_wire == 0 && interval_slots == 0 {
        return Err(HydraError::InvalidSchedule.into());
    }

    let authority_signer: u8 = (payer.address().as_array() == authority) as u8;

    // Re-serializing each scheduled ix into the instructions-sysvar tail layout
    // widens its `num_accounts` from u8 to u16 — exactly one extra byte per ix —
    // so the tail length is `body_len + num_instructions`, which is at most
    // `body_len + MAX_INSTRUCTIONS`. Create the account at that O(1) upper bound,
    // then validate + serialize the tail in a single pass and shrink to the
    // exact length. This avoids a separate pass just to pre-compute the size.
    let body_len = data.len() - CREATE_FIXED_PREFIX_LEN;
    let upper_region = body_len + MAX_INSTRUCTIONS;
    // The exact tail length (`<= upper_region`) is stored in a `u16` header field.
    if upper_region > u16::MAX as usize {
        return Err(HydraError::InvalidSchedule.into());
    }

    // Derive expected PDA and verify match.
    let (expected_pda, bump) =
        Address::find_program_address(&[CRANK_SEED_PREFIX, seed.as_ref()], &hydra_api::ID);
    if crank_ai.address() != &expected_pda {
        return Err(ProgramError::InvalidSeeds);
    }

    let total_size = CRANK_HEADER_SIZE + upper_region;

    // One sysvar read serves both CreateAccount funding and the cached floor.
    let rent = Rent::get()?;
    let rent_min = rent.try_minimum_balance(total_size)?;

    // Sign the CreateAccount with the PDA's seeds so it owns itself on creation.
    let bump_arr = [bump];
    let seeds_arr = [
        Seed::from(CRANK_SEED_PREFIX),
        Seed::from(seed.as_ref()),
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
            owner: &hydra_api::ID,
            funding: (funding_lamports > 0).then_some(Funding {
                from: payer,
                lamports: funding_lamports,
            }),
        }
        .invoke_signed(&signers)?;
    }

    #[cfg(not(feature = "create-account-allow-prefund"))]
    {
        let funding_lamports = rent_min.saturating_sub(crank_ai.lamports());
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
            owner: &hydra_api::ID,
        }
        .invoke_signed(&signers)?;
    }

    // Validate each scheduled ix and re-serialize it into the tail in the
    // instructions-sysvar wire layout, counting and accumulating the exact
    // length as we go. The account is over-provisioned to `upper_region`, so
    // every write stays in bounds (the tail is at most one byte per ix longer
    // than the wire body, and there are at most `MAX_INSTRUCTIONS` of them).
    let region_len = {
        let mut account_data = crank_ai.try_borrow_mut()?;
        let buf: &mut [u8] = &mut account_data;
        let (header_bytes, tail_bytes) = buf.split_at_mut(CRANK_HEADER_SIZE);

        let mut cursor = CREATE_FIXED_PREFIX_LEN;
        let mut off = 0usize;
        let mut num_instructions = 0usize;
        while cursor < data.len() {
            if cursor + CREATE_IX_HEADER_LEN > data.len() {
                return Err(ProgramError::InvalidInstructionData);
            }
            num_instructions += 1;
            if num_instructions > MAX_INSTRUCTIONS {
                return Err(HydraError::InvalidSchedule.into());
            }
            let num_accounts = data[cursor] as usize;
            let data_len = u16::from_le_bytes([data[cursor + 1], data[cursor + 2]]) as usize;
            if num_accounts > MAX_ACCOUNTS || data_len > MAX_DATA_LEN {
                return Err(HydraError::InvalidSchedule.into());
            }
            let metas_offset = cursor + CREATE_IX_HEADER_LEN;
            let metas_len = num_accounts * SERIALIZED_META_SIZE;
            let data_offset = metas_offset + metas_len;
            let next = data_offset + data_len;
            if next > data.len() {
                return Err(ProgramError::InvalidInstructionData);
            }
            // Reject any signer flag — scheduled ixs run top-level, they can
            // only be signed by real keys, and this program can't produce a
            // signature for a declared pubkey anyway.
            for i in 0..num_accounts {
                if data[metas_offset + i * SERIALIZED_META_SIZE] & META_FLAG_SIGNER != 0 {
                    return Err(HydraError::SignerInScheduledIx.into());
                }
            }
            // Tail blob, instructions-sysvar layout:
            // [num_accounts u16][metas][program_id 32][data_len u16][data]
            tail_bytes[off..off + 2].copy_from_slice(&(num_accounts as u16).to_le_bytes());
            off += 2;
            tail_bytes[off..off + metas_len].copy_from_slice(&data[metas_offset..data_offset]);
            off += metas_len;
            tail_bytes[off..off + 32]
                .copy_from_slice(&data[cursor + 3..cursor + CREATE_IX_HEADER_LEN]);
            off += 32;
            tail_bytes[off..off + 2].copy_from_slice(&(data_len as u16).to_le_bytes());
            off += 2;
            tail_bytes[off..off + data_len].copy_from_slice(&data[data_offset..next]);
            off += data_len;

            cursor = next;
        }

        // SAFETY: split yields CRANK_HEADER_SIZE bytes; Crank is align-1 (compile-time checked).
        let state = unsafe { load_crank_mut(header_bytes)? };
        state.authority = *authority;
        state.seed = *seed;
        state.set_next_exec_slot(start_slot);
        state.set_interval_slots(interval_slots);
        state.set_remaining(if remaining_wire == 0 {
            REMAINING_INFINITE
        } else {
            remaining_wire
        });
        state.set_priority_tip(priority_tip);
        state.set_executed(0);
        state.set_rent_min(rent_min);
        state.set_region_len(off as u16);
        state.bump = bump;
        state.set_cu_limit(cu_limit);
        state.authority_signer = authority_signer;

        off
    };

    // Trim the over-provisioned account down to its exact size. Shrinking only
    // lowers the data-length field — no syscall, never out of bounds — so the
    // unchecked variant is safe: `region_len <= upper_region`, the borrow above
    // is dropped, and the crank is program-owned after `CreateAccount`.
    unsafe {
        crank_ai.resize(CRANK_HEADER_SIZE + region_len);
    }

    // Suppress unused-import warnings when `logging` feature is off.
    let _ = _ix::CREATE;

    Ok(())
}

#[inline(always)]
fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    // SAFETY: the caller ensures `offset + 8 <= data.len()`.
    unsafe { u64::from_le_bytes(*(data.as_ptr().add(offset) as *const [u8; 8])) }
}

#[inline(always)]
fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    // SAFETY: the caller ensures `offset + 4 <= data.len()`.
    unsafe { u32::from_le_bytes(*(data.as_ptr().add(offset) as *const [u8; 4])) }
}
