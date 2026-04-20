//! Tiny utilities that don't fit into any one processor.

use pinocchio::error::ProgramError;
#[cfg(target_os = "solana")]
use pinocchio::sysvars::clock::CLOCK_ID;

/// Transaction-top-level stack height. Any CPI increments this.
pub const TRANSACTION_LEVEL_STACK_HEIGHT: u64 = 1;

/// Current stack height, via the raw syscall. Pinocchio 0.11 doesn't expose
/// this directly, so we call it through `solana-define-syscall`.
#[inline(always)]
pub fn get_stack_height() -> u64 {
    #[cfg(target_os = "solana")]
    unsafe {
        solana_define_syscall::definitions::sol_get_stack_height()
    }
    #[cfg(not(target_os = "solana"))]
    TRANSACTION_LEVEL_STACK_HEIGHT
}

/// Read just the `slot` field of the Clock sysvar — one syscall, no stack copy.
#[inline(always)]
pub fn get_clock_slot() -> Result<u64, ProgramError> {
    #[cfg(target_os = "solana")]
    {
        let mut slot: u64 = 0;
        // SAFETY: `slot` is a stack local, writing 8 bytes is in-bounds.
        let rc = unsafe {
            solana_define_syscall::definitions::sol_get_sysvar(
                CLOCK_ID.as_array().as_ptr(),
                &mut slot as *mut u64 as *mut u8,
                0,
                8,
            )
        };
        if rc != 0 {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(slot)
    }
    #[cfg(not(target_os = "solana"))]
    {
        // Used only by host-side `cargo check`.
        Ok(0)
    }
}
