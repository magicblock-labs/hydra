//! `Trigger` (disc 1) — the hot path.
//!
//! Accounts: `[crank(w), cranker(w,s), instructions_sysvar]`
//!
//! Binding to the scheduled instruction is via the instructions sysvar:
//! the cranker's tx must include, at `current_ix_index + 1`, an ix whose
//! serialized region matches the crank's stored tail byte-for-byte.

use pinocchio::{
    error::ProgramError, sysvars::instructions::INSTRUCTIONS_ID, AccountView, ProgramResult,
};

use hydra_api::{
    consts::{CRANKER_REWARD, CRANK_HEADER_SIZE, REMAINING_INFINITE},
    HydraError,
};

use crate::helpers::{get_clock_slot, get_stack_height, TRANSACTION_LEVEL_STACK_HEIGHT};

// Field offsets inside the 120-byte `Crank` header, kept local to this file
// so the raw reads below stay easy to verify against the account layout.
const OFF_NEXT_EXEC_SLOT: usize = 64;
const OFF_INTERVAL_SLOTS: usize = 72;
const OFF_REMAINING: usize = 80;
const OFF_PRIORITY_TIP: usize = 88;
const OFF_EXECUTED: usize = 96;
const OFF_RENT_MIN: usize = 104;
const OFF_REGION_LEN: usize = 112;

pub fn process(accounts: &mut [AccountView], _data: &[u8]) -> ProgramResult {
    cu_mark(); // 0  — before any work

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

    cu_mark(); // 1  — account-shape checks done

    // Top-level only — required for the `current_ix_index + 1` trick to be
    // well-defined. CPI would report the parent's index in the sysvar.
    if get_stack_height() != TRANSACTION_LEVEL_STACK_HEIGHT {
        return Err(HydraError::InvalidInstruction.into());
    }

    if crank_ai.data_len() < CRANK_HEADER_SIZE {
        return Err(ProgramError::AccountDataTooSmall);
    }

    // Fetch just the 8-byte `slot` field of Clock — one syscall, no stack copy.
    let current_slot = get_clock_slot()?;

    cu_mark(); // 2  — after stack-height + clock syscalls

    // Read the 7 header fields we actually need, directly from the account
    // data, skipping the struct copy + pinocchio RefCell bookkeeping.
    let hdr = unsafe { read_header(crank_ai.data_ptr()) };

    if current_slot < hdr.next_exec_slot {
        return Err(HydraError::NotYetExecutable.into());
    }
    if hdr.remaining == 0 {
        return Err(HydraError::Exhausted.into());
    }

    let reward = CRANKER_REWARD
        .checked_add(hdr.priority_tip)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    let new_crank_lamports = crank_ai
        .lamports()
        .checked_sub(reward)
        .ok_or::<ProgramError>(HydraError::InsufficientFunds.into())?;
    if new_crank_lamports < hdr.rent_min {
        return Err(HydraError::InsufficientFunds.into());
    }

    cu_mark(); // 3  — header + schedule + reward math done

    verify_followup(ix_sysvar_ai, crank_ai, hdr.region_len as usize)?;

    cu_mark(); // 4  — sysvar verify done

    // Pay cranker via direct lamport mutation.
    crank_ai.set_lamports(new_crank_lamports);
    let new_cranker_lamports = cranker_ai
        .lamports()
        .checked_add(reward)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    cranker_ai.set_lamports(new_cranker_lamports);

    // Advance schedule + exhaust counter via raw-pointer writes.
    let next_slot = hdr
        .next_exec_slot
        .checked_add(hdr.interval_slots)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    // SAFETY: `crank_ai` is owned by this program and has at least
    // `CRANK_HEADER_SIZE` bytes (checked above). No other borrow is live.
    unsafe {
        let p = crank_ai.data_mut_ptr();
        write_u64(p.add(OFF_NEXT_EXEC_SLOT), next_slot);
        write_u64(p.add(OFF_EXECUTED), hdr.executed + 1);
        if hdr.remaining != REMAINING_INFINITE {
            write_u64(p.add(OFF_REMAINING), hdr.remaining - 1);
        }
    }

    cu_mark(); // 5  — end of Trigger
    Ok(())
}

/// CU checkpoint. No-op when `cu-trace` is off so release builds pay
/// zero overhead. Each call under `cu-trace` costs ~110 CU (one syscall).
#[inline(always)]
fn cu_mark() {
    #[cfg(all(feature = "cu-trace", target_os = "solana"))]
    unsafe {
        solana_define_syscall::definitions::sol_log_compute_units_();
    }
}

/// Subset of `Crank` that the Trigger hot path actually reads.
struct Snapshot {
    next_exec_slot: u64,
    interval_slots: u64,
    remaining: u64,
    priority_tip: u64,
    executed: u64,
    rent_min: u64,
    region_len: u16,
}

/// Unaligned-safe raw reads from the crank account data. Saves the ~120-byte
/// struct memcpy a naive `*state` would do.
///
/// # Safety
/// `p` must point to at least `CRANK_HEADER_SIZE` bytes of the crank account's
/// data region.
#[inline(always)]
unsafe fn read_header(p: *const u8) -> Snapshot {
    Snapshot {
        next_exec_slot: read_u64(p.add(OFF_NEXT_EXEC_SLOT)),
        interval_slots: read_u64(p.add(OFF_INTERVAL_SLOTS)),
        remaining: read_u64(p.add(OFF_REMAINING)),
        priority_tip: read_u64(p.add(OFF_PRIORITY_TIP)),
        executed: read_u64(p.add(OFF_EXECUTED)),
        rent_min: read_u64(p.add(OFF_RENT_MIN)),
        region_len: read_u16(p.add(OFF_REGION_LEN)),
    }
}

/// Parse the instructions sysvar, locate the region for
/// `current_ix_index + 1`, and byte-compare it against the crank's stored tail.
#[inline(always)]
fn verify_followup(sysvar: &AccountView, crank: &AccountView, region_len: usize) -> ProgramResult {
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
unsafe fn read_u64(p: *const u8) -> u64 {
    core::ptr::read_unaligned(p as *const u64)
}

#[inline(always)]
unsafe fn read_u16(p: *const u8) -> u16 {
    core::ptr::read_unaligned(p as *const u16)
}

#[inline(always)]
unsafe fn write_u64(p: *mut u8, v: u64) {
    core::ptr::write_unaligned(p as *mut u64, v);
}
