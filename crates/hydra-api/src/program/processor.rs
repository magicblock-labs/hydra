//! Shared helpers for the on-chain crank processors (base + ephemeral).
//!
//! These building blocks are program-agnostic: every function that needs the
//! caller's program id takes it as a parameter, so both the base-layer and the
//! ephemeral-rollup programs link the exact same code here.

use pinocchio::{error::ProgramError, AccountView, Address, ProgramResult};

use crate::{
    consts::{
        self, CRANK_SEED_PREFIX, MAX_ACCOUNTS, MAX_COMPUTE_UNIT_LIMIT, MAX_DATA_LEN,
        MAX_INSTRUCTIONS, META_FLAG_SIGNER, REMAINING_INFINITE, SERIALIZED_META_SIZE,
    },
    instruction::{CREATE_FIXED_PREFIX_LEN, CREATE_IX_HEADER_LEN},
    program::helpers::get_clock_slot,
    state::{load_crank, load_crank_mut, Crank},
    HydraError, CRANK_HEADER_SIZE,
};

/// `signer` must sign and `crank` must be Hydra-owned — the preamble of every
/// `Cancel` / `Close` path.
pub fn require_signed_crank(
    signer: &AccountView,
    crank_ai: &AccountView,
    program_id: &Address,
) -> ProgramResult {
    if !signer.is_signer() {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if !crank_ai.owned_by(program_id) {
        return Err(ProgramError::InvalidAccountOwner);
    }
    Ok(())
}

/// Anti-grief: when a crank has a non-zero authority, only that authority may
/// receive the rent refund. Shared by base and ephemeral `Close`.
pub fn require_refund_recipient(
    stored_authority: [u8; 32],
    recipient: &AccountView,
) -> ProgramResult {
    if stored_authority != [0u8; 32] && recipient.address().as_array() != &stored_authority {
        return Err(HydraError::UnauthorizedAuthority.into());
    }
    Ok(())
}

/// Authority-gated close preamble shared by base `Cancel` and `CancelEphemeral`:
/// `authority` signs a Hydra-owned crank and matches its stored (non-zero)
/// authority.
pub fn require_cancel_authority(
    authority: &AccountView,
    crank_ai: &AccountView,
    program_id: &Address,
) -> ProgramResult {
    require_signed_crank(authority, crank_ai, program_id)?;
    let stored = {
        let data = crank_ai.try_borrow()?;
        unsafe { load_crank(&data)? }.authority
    };
    if stored == [0u8; 32] || authority.address().as_array() != &stored {
        return Err(HydraError::UnauthorizedAuthority.into());
    }
    Ok(())
}

/// The fixed-size scheduling prefix of a `Create` / `CreateEphemeral` payload.
/// `next_exec` / `interval` are slot counts on both ledgers — base-layer slots
/// for base cranks, ephemeral-rollup slots for ephemeral cranks (the ER runs
/// faster, so the same wall-clock interval is more slots). Both `Trigger`
/// handlers compare against the ledger's own clock slot and advance
/// `next_exec_slot` by `interval_slots`; the wire bytes are identical either way.
pub struct CreateHeader {
    pub seed: [u8; 32],
    pub authority: [u8; 32],
    pub next_exec: u64,
    pub interval: u64,
    pub remaining_wire: u64,
    pub priority_tip: u64,
    pub cu_limit: u32,
}

/// Parse + validate the fixed prefix of a `Create` payload. Requires at least
/// the prefix plus one scheduled-ix blob header.
pub fn parse_create_header(data: &[u8]) -> Result<CreateHeader, ProgramError> {
    if data.len() < CREATE_FIXED_PREFIX_LEN + CREATE_IX_HEADER_LEN {
        return Err(ProgramError::InvalidInstructionData);
    }
    // SAFETY: bounds checked above; the reads only require byte alignment.
    let header = CreateHeader {
        seed: unsafe { *(data.as_ptr() as *const [u8; 32]) },
        authority: unsafe { *(data.as_ptr().add(32) as *const [u8; 32]) },
        next_exec: read_u64_le(data, 64),
        interval: read_u64_le(data, 72),
        remaining_wire: read_u64_le(data, 80),
        priority_tip: read_u64_le(data, 88),
        cu_limit: read_u32_le(data, 96),
    };
    // `0` opts out of `SetComputeUnitLimit`; non-zero must fit the per-tx ceiling.
    if header.cu_limit > MAX_COMPUTE_UNIT_LIMIT {
        return Err(HydraError::InvalidSchedule.into());
    }
    // A never-advancing infinite crank makes no sense.
    if header.remaining_wire == 0 && header.interval == 0 {
        return Err(HydraError::InvalidSchedule.into());
    }
    Ok(header)
}

/// Write the parsed prefix + computed fields into a freshly-allocated crank
/// header. `rent_min` is the cached rent floor for base cranks and `0` for
/// ephemeral cranks (which hold no lamports).
#[inline(always)]
pub fn write_header(
    state: &mut Crank,
    h: &CreateHeader,
    bump: u8,
    authority_signer: u8,
    rent_min: u64,
    region_len: u16,
) {
    state.authority = h.authority;
    state.seed = h.seed;
    state.set_next_exec_slot(h.next_exec);
    state.set_interval_slots(h.interval);
    state.set_remaining(if h.remaining_wire == 0 {
        REMAINING_INFINITE
    } else {
        h.remaining_wire
    });
    state.set_priority_tip(h.priority_tip);
    state.set_executed(0);
    state.set_rent_min(rent_min);
    state.set_region_len(region_len);
    state.bump = bump;
    state.set_cu_limit(h.cu_limit);
    state.authority_signer = authority_signer;
}

/// The scheduled-ix tail must fit `Crank.region_len` (a `u16`).
const MAX_REGION_LEN: usize = u16::MAX as usize;

/// Measure the exact tail length the scheduled ixs serialize to, validating the
/// per-ix structure, the instruction-count limit, and the `region_len` ceiling
/// in a single pass. Mirrors the byte accounting in [`write_tail`] so the caller
/// can allocate the precise account size up front; `write_tail` then re-validates
/// (incl. signer flags) and writes, yielding the same length.
pub fn measure_region(data: &[u8]) -> Result<usize, ProgramError> {
    let mut cursor = CREATE_FIXED_PREFIX_LEN;
    let mut region_len = 0usize;
    let mut num_instructions = 0usize;

    while cursor < data.len() {
        let (num_accounts, data_len, next) = parse_ix_header(data, cursor)?;
        num_instructions += 1;
        if num_instructions > MAX_INSTRUCTIONS {
            return Err(HydraError::InvalidSchedule.into());
        }
        let metas_len = num_accounts * SERIALIZED_META_SIZE;
        // [num_accounts u16][metas][program_id 32][data_len u16][data]
        region_len += 2 + metas_len + 32 + 2 + data_len;
        cursor = next;
    }

    if region_len > MAX_REGION_LEN {
        return Err(HydraError::InvalidSchedule.into());
    }
    Ok(region_len)
}

/// Derive the crank PDA from `[CRANK_SEED_PREFIX, seed]` and verify it matches
/// the supplied account, returning the bump for the create CPI's signer seeds.
/// Shared by both `Create` paths.
pub fn derive_crank_pda(
    crank_ai: &AccountView,
    seed: &[u8; 32],
    program_id: &Address,
) -> Result<u8, ProgramError> {
    let (expected_pda, bump) =
        Address::find_program_address(&[CRANK_SEED_PREFIX, seed], program_id);
    if crank_ai.address() != &expected_pda {
        return Err(ProgramError::InvalidSeeds);
    }
    Ok(bump)
}

/// Finalize a freshly-allocated crank account: serialize the scheduled-ix tail
/// and write the header. The account must already be sized to
/// `CRANK_HEADER_SIZE + region_len`; `rent_min` is the cached rent floor for base
/// cranks and `0` for ephemeral cranks (which hold no lamports). Shared by both
/// `Create` paths.
pub fn write_crank(
    crank_ai: &AccountView,
    data: &[u8],
    header: &CreateHeader,
    bump: u8,
    authority_signer: u8,
    rent_min: u64,
    region_len: usize,
) -> ProgramResult {
    let mut account_data = crank_ai.try_borrow_mut()?;
    let buf: &mut [u8] = &mut account_data;
    if buf.len() < CRANK_HEADER_SIZE {
        return Err(ProgramError::AccountDataTooSmall);
    }
    let (header_bytes, tail_bytes) = buf.split_at_mut(CRANK_HEADER_SIZE);

    let written = write_tail(tail_bytes, data)?;
    if written != region_len {
        return Err(HydraError::InvalidSchedule.into());
    }

    // SAFETY: split yields CRANK_HEADER_SIZE bytes; Crank is align-1 (compile-time checked).
    let state = unsafe { load_crank_mut(header_bytes)? };
    write_header(
        state,
        header,
        bump,
        authority_signer,
        rent_min,
        region_len as u16,
    );
    Ok(())
}

/// Validate + serialize the scheduled ixs of a `Create` payload into `tail`,
/// returning the bytes written. Every write is bounds-checked against
/// `tail.len()` so a wrongly-sized account fails cleanly instead of writing out
/// of bounds.
///
/// Mirrors the tail layout produced by `processor::create` — see that file for
/// the wire format. Kept separate so the audited base path stays untouched.
pub fn write_tail(tail: &mut [u8], data: &[u8]) -> Result<usize, ProgramError> {
    let cap = tail.len();
    let mut cursor = CREATE_FIXED_PREFIX_LEN;
    let mut off = 0usize;
    let mut num_instructions = 0usize;

    while cursor < data.len() {
        let (num_accounts, data_len, next) = parse_ix_header(data, cursor)?;
        num_instructions += 1;
        if num_instructions > MAX_INSTRUCTIONS {
            return Err(HydraError::InvalidSchedule.into());
        }
        let metas_offset = cursor + CREATE_IX_HEADER_LEN;
        let metas_len = num_accounts * SERIALIZED_META_SIZE;
        let data_offset = metas_offset + metas_len;
        // For each meta: reject signer flags (scheduled ixs run top-level and
        // can only be signed by real keys, which this program can't produce),
        // and fold the account into the writability-conflict tracker.
        for account_index in 0..num_accounts {
            let meta_offset = metas_offset + account_index * SERIALIZED_META_SIZE;
            let meta_flag = data[meta_offset];
            if meta_flag & META_FLAG_SIGNER != 0 {
                return Err(HydraError::SignerInScheduledIx.into());
            }
        }

        // [num_accounts u16][metas][program_id 32][data_len u16][data]
        let blob_len = 2 + metas_len + 32 + 2 + data_len;
        if off + blob_len > cap {
            return Err(ProgramError::AccountDataTooSmall);
        }
        tail[off..off + 2].copy_from_slice(&(num_accounts as u16).to_le_bytes());
        off += 2;
        tail[off..off + metas_len].copy_from_slice(&data[metas_offset..data_offset]);
        off += metas_len;
        tail[off..off + 32].copy_from_slice(&data[cursor + 3..cursor + CREATE_IX_HEADER_LEN]);
        off += 32;
        tail[off..off + 2].copy_from_slice(&(data_len as u16).to_le_bytes());
        off += 2;
        tail[off..off + data_len].copy_from_slice(&data[data_offset..next]);
        off += data_len;

        cursor = next;
    }

    Ok(off)
}

/// Parse one scheduled-ix blob header at `cursor`, validating limits and bounds.
/// Returns `(num_accounts, data_len, next_cursor)`.
#[inline(always)]
pub fn parse_ix_header(data: &[u8], cursor: usize) -> Result<(usize, usize, usize), ProgramError> {
    if cursor + CREATE_IX_HEADER_LEN > data.len() {
        return Err(ProgramError::InvalidInstructionData);
    }
    let num_accounts = data[cursor] as usize;
    let data_len = u16::from_le_bytes([data[cursor + 1], data[cursor + 2]]) as usize;
    if num_accounts > MAX_ACCOUNTS || data_len > MAX_DATA_LEN {
        return Err(HydraError::InvalidSchedule.into());
    }
    let metas_len = num_accounts * SERIALIZED_META_SIZE;
    let next = cursor + CREATE_IX_HEADER_LEN + metas_len + data_len;
    if next > data.len() {
        return Err(ProgramError::InvalidInstructionData);
    }
    Ok((num_accounts, data_len, next))
}

/// Parse the instructions sysvar, locate the region for
/// `current_ix_index + 1`, and byte-compare it against the crank's stored tail.
///
/// Shared with the ephemeral-rollup `TriggerEphemeral` handler — the
/// follow-up binding is identical on both ledgers.
#[inline(always)]
pub fn verify_followup(
    sysvar: &AccountView,
    crank: &AccountView,
    region_len: usize,
) -> ProgramResult {
    // SAFETY: we're in a linear entrypoint flow with no outstanding borrows
    // on either account. pinocchio's `borrow_unchecked` skips the refcell
    // bookkeeping, which saves a handful of CUs per call.
    let sv: &[u8] = unsafe { sysvar.borrow_unchecked() };
    let cr: &[u8] = unsafe { crank.borrow_unchecked() };

    let sv_len = sv.len();
    if sv_len < 4 {
        return Err(ProgramError::InvalidAccountData);
    }

    // [len-2..len] = current_ix_index (u16 LE)
    let current = unsafe { read_u16(sv.as_ptr().add(sv_len - 2)) } as usize;
    let target = current
        .checked_add(1)
        .ok_or(HydraError::MissingFollowupInstruction)?;

    // [0..2] = num_instructions (u16 LE)
    let num_ix = unsafe { read_u16(sv.as_ptr()) } as usize;
    if target >= num_ix {
        return Err(HydraError::MissingFollowupInstruction.into());
    }

    // [2 + 2*target..+2] = offset of instruction `target`'s region.
    let off_pos = 2 + 2 * target;
    if off_pos + 2 > sv_len {
        return Err(ProgramError::InvalidAccountData);
    }
    let region_start = unsafe { read_u16(sv.as_ptr().add(off_pos)) } as usize;
    let region_end = region_start
        .checked_add(region_len)
        .ok_or(HydraError::MismatchedFollowupIx)?;
    if region_end > sv_len.saturating_sub(2) {
        return Err(HydraError::MismatchedFollowupIx.into());
    }

    let tail_end = CRANK_HEADER_SIZE
        .checked_add(region_len)
        .ok_or(ProgramError::InvalidAccountData)?;
    if tail_end > cr.len() {
        return Err(ProgramError::InvalidAccountData);
    }
    let tail = &cr[CRANK_HEADER_SIZE..tail_end];
    let sv_region = &sv[region_start..region_end];

    if sv_region != tail {
        return Err(HydraError::MismatchedFollowupIx.into());
    }
    Ok(())
}

#[inline(always)]
pub fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    // SAFETY: the caller ensures `offset + 8 <= data.len()`.
    unsafe { u64::from_le_bytes(*(data.as_ptr().add(offset) as *const [u8; 8])) }
}

#[inline(always)]
pub fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    // SAFETY: the caller ensures `offset + 4 <= data.len()`.
    unsafe { u32::from_le_bytes(*(data.as_ptr().add(offset) as *const [u8; 4])) }
}

/// # Safety
/// `p` must point to at least 2 readable bytes; the read is unaligned-safe.
#[inline(always)]
pub unsafe fn read_u16(p: *const u8) -> u16 {
    core::ptr::read_unaligned(p as *const u16)
}

/// Move all lamports out of `src` into `dst`.
#[inline(always)]
pub fn drain_lamports(src: &AccountView, dst: &AccountView) -> ProgramResult {
    let amount = src.lamports();
    let new_dst = dst
        .lamports()
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    src.set_lamports(0);
    dst.set_lamports(new_dst);
    Ok(())
}

/// Shared `Cancel`: authority-gated drain of the crank's full balance to
/// `recipient`.
pub fn process_cancel(
    authority: &AccountView,
    crank_ai: &AccountView,
    recipient: &AccountView,
    program_id: &Address,
) -> ProgramResult {
    require_cancel_authority(authority, crank_ai, program_id)?;
    drain_lamports(crank_ai, recipient)
}

/// Shared `Close`: permissionless cleanup of an exhausted / underfunded / stuck
/// crank. Pays a flat `CRANKER_REWARD` bounty to `reporter`, refunds the
/// remaining balance to `recipient` (anti-grief: bound to the stored authority
/// when set), and zeroes the crank.
pub fn process_close(
    reporter: &AccountView,
    crank_ai: &AccountView,
    recipient: &AccountView,
    program_id: &Address,
    is_ephemeral: bool,
) -> ProgramResult {
    let cranker_reward = if is_ephemeral {
        consts::ephemeral::CRANKER_REWARD
    } else {
        consts::base::CRANKER_REWARD
    };
    let staleness_threshold_slots = if is_ephemeral {
        consts::ephemeral::STALENESS_THRESHOLD_SLOTS
    } else {
        consts::base::STALENESS_THRESHOLD_SLOTS
    };

    require_signed_crank(reporter, crank_ai, program_id)?;

    // Snapshot the fields we need from the crank header.
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
    let next_reward = cranker_reward
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
    let stuck = current_slot.saturating_sub(next_exec_slot) > staleness_threshold_slots;

    if !(exhausted || underfunded || stuck) {
        return Err(HydraError::NotClosable.into());
    }

    require_refund_recipient(stored_authority, recipient)?;

    // Flat bounty (2 × base fee) to whoever cranked the cleanup; the balance
    // refunds to `recipient`. `min` handles a crank holding less than the
    // bounty — reporter gets what's there, recipient gets nothing.
    let bounty = cranker_reward.min(lamports_now);
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
