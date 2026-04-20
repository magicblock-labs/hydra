//! Numeric constants, feature bits, and bounds that both the on-chain
//! program and its clients reference.

/// Seed prefix for the Crank PDA: `[b"crank", seed_bytes]`.
pub const CRANK_SEED_PREFIX: &[u8] = b"crank";

/// Solana base transaction fee (lamports per signature).
pub const BASE_FEE_LAMPORTS: u64 = 5_000;

/// Flat per-trigger reward paid to the cranker. Equals `2 × base_fee`.
pub const CRANKER_REWARD: u64 = 2 * BASE_FEE_LAMPORTS;

/// Max metas the scheduled ix may declare.
pub const MAX_ACCOUNTS: usize = 32;

/// Max bytes of the scheduled ix's `data` field.
pub const MAX_DATA_LEN: usize = 1024;

/// Internal sentinel in `Crank.remaining` meaning "execute forever".
/// Wire-level `0` is converted to this at `Create`.
pub const REMAINING_INFINITE: u64 = u64::MAX;

/// Fixed header size, exactly `core::mem::size_of::<Crank>()`.
pub const CRANK_HEADER_SIZE: usize = 120;

/// Max `cu_limit` a crank may declare. Solana's per-tx ceiling is 1.4 M CU,
/// so anything larger would be rejected by the runtime anyway. `0` means
/// "don't emit `SetComputeUnitLimit`" (inherits the 200 k/ix default).
pub const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// Slots of overdue past `next_exec_slot` after which a crank is considered
/// stuck and `Close` becomes permissionlessly callable. `next_exec_slot` only
/// advances on successful `Trigger`, so a crank whose inner ix deterministically
/// fails (or whose target is paused) would otherwise pin its rent forever.
/// ~31 days at 400 ms/slot: 31 × 86_400 / 0.4 = 6_696_000.
pub const STALENESS_THRESHOLD_SLOTS: u64 = 6_696_000;

/// Per-meta size in both the on-chain template bytes and the instructions
/// sysvar wire format: `[1 flag byte][32-byte pubkey]`.
pub const SERIALIZED_META_SIZE: usize = 33;

/// Instructions-sysvar per-meta flag: `is_signer` bit.
pub const META_FLAG_SIGNER: u8 = 0b0000_0001;

/// Instructions-sysvar per-meta flag: `is_writable` bit.
pub const META_FLAG_WRITABLE: u8 = 0b0000_0010;

/// Instruction discriminators. One byte each, matched in the entrypoint.
pub mod ix {
    pub const CREATE: u8 = 0;
    pub const TRIGGER: u8 = 1;
    pub const CANCEL: u8 = 2;
    pub const CLOSE: u8 = 3;
}
