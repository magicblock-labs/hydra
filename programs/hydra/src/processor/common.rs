//! Shared helpers for the base-layer crank processors.

use pinocchio::{error::ProgramError, AccountView, ProgramResult};

/// Move all lamports out of `src` into `dst`. The runtime zeroes `src.data`
/// and reassigns ownership to the system program at the instruction boundary
/// because `src.lamports == 0` post-write.
#[inline(always)]
pub(super) fn drain_lamports(src: &AccountView, dst: &AccountView) -> ProgramResult {
    let amount = src.lamports();
    let new_dst = dst
        .lamports()
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    src.set_lamports(0);
    dst.set_lamports(new_dst);
    Ok(())
}

#[inline(always)]
pub(super) unsafe fn read_u64(p: *const u8) -> u64 {
    core::ptr::read_unaligned(p as *const u64)
}

#[inline(always)]
pub(super) unsafe fn write_u64(p: *mut u8, v: u64) {
    core::ptr::write_unaligned(p as *mut u64, v);
}
