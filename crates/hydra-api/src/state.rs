//! On-chain `Crank` account layout and zero-copy helpers.
//!
//! Layout:
//!
//! ```text
//! [0..120)                header  — #[repr(C)], align-1, exactly 120 bytes
//! [120..120+region_len)   region  — sysvar-format bytes of the scheduled ix,
//!                                   stored verbatim so that verification
//!                                   collapses to one memcmp against the
//!                                   instructions-sysvar region.
//! ```
//!
//! Region bytes (matching the instructions-sysvar per-ix wire layout):
//!
//! ```text
//! [0..2]                        num_accounts           u16 LE
//! [2..2 + 33*num_accounts]      metas: [flag:u8][pubkey:[u8;32]] each
//!                                flag bit 0 = is_signer  (MUST be 0; rejected at Create)
//!                                flag bit 1 = is_writable
//! [+32]                         program_id             [u8; 32]
//! [+2]                          data_len               u16 LE
//! [..data_len]                  data                   bytes
//! ```

use pinocchio::error::ProgramError;

use crate::consts::{CRANK_HEADER_SIZE, SERIALIZED_META_SIZE};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Crank {
    /// All-zeros = no cancel authority (crank is fire-and-forget until it
    /// exhausts naturally or `Close` recovers rent permissionlessly).
    pub authority: [u8; 32],
    /// The 32-byte seed bytes that derived this PDA (`[b"crank", seed]`).
    pub seed: [u8; 32],
    pub next_exec_slot: [u8; 8],
    pub interval_slots: [u8; 8],
    /// `REMAINING_INFINITE` = execute forever. Otherwise decremented each
    /// trigger until it hits 0 (exhausted).
    pub remaining: [u8; 8],
    pub priority_tip: [u8; 8],
    pub executed: [u8; 8],
    /// Cached `Rent::minimum_balance(total_size)` set at `Create`.
    pub rent_min: [u8; 8],
    /// Size of the tail region in bytes.
    pub region_len: [u8; 2],
    pub bump: u8,
    /// Compute-unit limit the cranker should emit as `SetComputeUnitLimit`
    /// before the `Trigger` ix. `0` = omit the ix (inherit the per-ix
    /// default). Set immutably at `Create`, capped at
    /// [`crate::consts::MAX_COMPUTE_UNIT_LIMIT`].
    pub cu_limit: [u8; 4],
    pub _pad: [u8; 1],
}

impl Crank {
    pub const LEN: usize = CRANK_HEADER_SIZE;

    #[inline(always)]
    pub fn next_exec_slot(&self) -> u64 {
        u64::from_le_bytes(self.next_exec_slot)
    }
    #[inline(always)]
    pub fn set_next_exec_slot(&mut self, v: u64) {
        self.next_exec_slot = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn interval_slots(&self) -> u64 {
        u64::from_le_bytes(self.interval_slots)
    }
    #[inline(always)]
    pub fn set_interval_slots(&mut self, v: u64) {
        self.interval_slots = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn remaining(&self) -> u64 {
        u64::from_le_bytes(self.remaining)
    }
    #[inline(always)]
    pub fn set_remaining(&mut self, v: u64) {
        self.remaining = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn priority_tip(&self) -> u64 {
        u64::from_le_bytes(self.priority_tip)
    }
    #[inline(always)]
    pub fn set_priority_tip(&mut self, v: u64) {
        self.priority_tip = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn executed(&self) -> u64 {
        u64::from_le_bytes(self.executed)
    }
    #[inline(always)]
    pub fn set_executed(&mut self, v: u64) {
        self.executed = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn rent_min(&self) -> u64 {
        u64::from_le_bytes(self.rent_min)
    }
    #[inline(always)]
    pub fn set_rent_min(&mut self, v: u64) {
        self.rent_min = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn region_len(&self) -> u16 {
        u16::from_le_bytes(self.region_len)
    }
    #[inline(always)]
    pub fn set_region_len(&mut self, v: u16) {
        self.region_len = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn cu_limit(&self) -> u32 {
        u32::from_le_bytes(self.cu_limit)
    }
    #[inline(always)]
    pub fn set_cu_limit(&mut self, v: u32) {
        self.cu_limit = v.to_le_bytes();
    }

    #[inline(always)]
    pub fn bump(&self) -> u8 {
        self.bump
    }
}

// Compile-time layout assertions: one-byte alignment + exact header size.
const _: () = {
    assert!(core::mem::size_of::<Crank>() == Crank::LEN);
    assert!(core::mem::align_of::<Crank>() == 1);
};

/// Total on-chain account size for a crank with a given tail region length.
#[inline(always)]
pub const fn crank_account_size(region_len: usize) -> usize {
    CRANK_HEADER_SIZE + region_len
}

/// Region length for a scheduled ix with `num_accounts` metas and `data_len` data bytes.
#[inline(always)]
pub const fn region_len_for(num_accounts: usize, data_len: usize) -> usize {
    // [num_accounts: u16][metas: 33*N][program_id: 32][data_len: u16][data]
    2 + SERIALIZED_META_SIZE * num_accounts + 32 + 2 + data_len
}

/// Cast raw account data to `&Crank`.
///
/// # Safety
/// Caller must guarantee the buffer belongs to a Hydra-owned account and
/// is live for the returned lifetime.
#[inline(always)]
pub unsafe fn load_crank(bytes: &[u8]) -> Result<&Crank, ProgramError> {
    if bytes.len() < Crank::LEN {
        return Err(ProgramError::AccountDataTooSmall);
    }
    Ok(&*(bytes.as_ptr() as *const Crank))
}

/// Cast raw account data to `&mut Crank`.
///
/// # Safety
/// As [`load_crank`], plus exclusive-access invariant for the returned borrow.
#[inline(always)]
pub unsafe fn load_crank_mut(bytes: &mut [u8]) -> Result<&mut Crank, ProgramError> {
    if bytes.len() < Crank::LEN {
        return Err(ProgramError::AccountDataTooSmall);
    }
    Ok(&mut *(bytes.as_mut_ptr() as *mut Crank))
}

/// Derive the crank PDA for the given 32-byte seed.
#[inline]
pub fn find_crank_pda(seed: &[u8; 32]) -> (solana_address::Address, u8) {
    solana_address::Address::find_program_address(
        &[crate::consts::CRANK_SEED_PREFIX, seed.as_ref()],
        &crate::ID,
    )
}
