//! Raw-pointer field helpers for the base-layer `Trigger` hot path. The lamport
//! drain + close/cancel payout logic is shared with the ephemeral program via
//! [`hydra_api::program::processor`].

/// Read/write the 8-byte header fields the `Trigger` hot path touches directly.
#[inline(always)]
pub(super) unsafe fn read_u64(p: *const u8) -> u64 {
    core::ptr::read_unaligned(p as *const u64)
}

#[inline(always)]
pub(super) unsafe fn write_u64(p: *mut u8, v: u64) {
    core::ptr::write_unaligned(p as *mut u64, v);
}
