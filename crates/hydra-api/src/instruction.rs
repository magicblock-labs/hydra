//! Instruction wire layouts + (behind `client` feature) host-side builders.
//!
//! # `Create` (disc 0) — wire layout
//!
//! ```text
//! seed:             [u8; 32]
//! authority:        [u8; 32]    // all-zeros = none
//! start_slot:       u64 LE
//! interval_slots:   u64 LE
//! remaining:        u64 LE      // 0 on the wire = infinite
//! priority_tip:     u64 LE
//! cu_limit:         u32 LE      // 0 = cranker omits SetComputeUnitLimit
//! ── one or more scheduled ixs, parsed until the data is exhausted: ──
//!   num_accounts:   u8
//!   data_len:       u16 LE
//!   program_id:     [u8; 32]
//!   metas:          [[flag:u8][pubkey:[u8;32]]; num_accounts]
//!   data:           [u8; data_len]
//! ```
//!
//! # `Trigger` (disc 1)   Accounts: `[crank(w), cranker(w,s), instructions_sysvar]`
//! # `Cancel`  (disc 2)   Accounts: `[authority(s), crank(w), recipient(w)]`
//! # `Close`   (disc 3)   Accounts: `[reporter(s,w), crank(w), recipient(w)]`
//!
//! To fund a crank after creation, send a direct `system_program::transfer`
//! to the crank PDA — no dedicated instruction is needed.

/// Fixed-size prefix of `Create` data, before variable data.
pub const CREATE_FIXED_PREFIX_LEN: usize = 32 +  // seed
    32 +  // authority
     8 +  // start_slot
     8 +  // interval_slots
     8 +  // remaining
     8 +  // priority_tip
     4; // cu_limit
        // = 100

/// Per-scheduled-ix fixed header in `Create` data, before its metas + data:
/// `num_accounts: u8`, `data_len: u16 LE`, `program_id: [u8; 32]`.
pub const CREATE_IX_HEADER_LEN: usize = 1 + 2 + 32; // = 35

// ---------------------------------------------------------------------------
// Client builders
// ---------------------------------------------------------------------------

#[cfg(feature = "client")]
mod client {
    extern crate alloc;
    use alloc::vec::Vec;

    use solana_instruction::{AccountMeta, Instruction};
    use solana_pubkey::{pubkey, Pubkey};

    use crate::consts::{ix, META_FLAG_WRITABLE};
    use crate::instruction::{CREATE_FIXED_PREFIX_LEN, CREATE_IX_HEADER_LEN};

    /// Solana's built-in instructions sysvar pubkey.
    pub const INSTRUCTIONS_SYSVAR_ID: Pubkey =
        pubkey!("Sysvar1nstructions1111111111111111111111111");

    /// Solana's system program pubkey.
    pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");

    /// The [`base`] / [`ephemeral`] programs.
    pub const BASE_PROGRAM_ID: Pubkey = pubkey!("Hydra17i1feui9deaxu6d1TzSQMRNHeBRkDR1Awy7zea");
    pub const EPHEMERAL_PROGRAM_ID: Pubkey = pubkey!("eHyd5BU8QffvHi4GnXwxrK4WpS7pM2x9UGKHBWii7mf");

    /// Hydra program ID as a `solana_pubkey::Pubkey` (convenience for clients).
    pub fn program_id() -> Pubkey {
        Pubkey::new_from_array(crate::ID.to_bytes())
    }

    /// A scheduled-ix meta as it will be stored on-chain.
    ///
    /// `is_signer` is intentionally absent: scheduled ixs cannot carry
    /// signer flags (enforced by `Create`), and the on-chain template
    /// stores only the writable bit anyway.
    #[derive(Clone, Copy)]
    pub struct SchedMeta {
        pub pubkey: Pubkey,
        pub is_writable: bool,
    }

    impl SchedMeta {
        pub fn readonly(pubkey: Pubkey) -> Self {
            Self {
                pubkey,
                is_writable: false,
            }
        }
        pub fn writable(pubkey: Pubkey) -> Self {
            Self {
                pubkey,
                is_writable: true,
            }
        }
    }

    /// One scheduled instruction template.
    pub struct ScheduledIx<'a> {
        pub program_id: Pubkey,
        pub metas: &'a [SchedMeta],
        pub data: &'a [u8],
    }

    /// Builders targeting the base-layer Hydra program ([`BASE_PROGRAM_ID`]).
    pub mod base {
        use super::*;

        /// This module's program ID.
        pub const PROGRAM_ID: Pubkey = super::BASE_PROGRAM_ID;

        /// Derive `(crank_pda, bump)` under the base program.
        pub fn find_crank_pda(seed: &[u8; 32]) -> (Pubkey, u8) {
            Pubkey::find_program_address(&[crate::consts::CRANK_SEED_PREFIX, seed], &PROGRAM_ID)
        }

        /// All the scheduling knobs for `Create`
        pub struct CreateArgs<'a> {
            pub seed: [u8; 32],
            /// All-zeros = unkillable (no cancel authority).
            pub authority: [u8; 32],
            pub start_slot: u64,
            pub interval_slots: u64,
            /// `0` on the wire means "infinite"; Hydra stores `u64::MAX` internally.
            pub remaining: u64,
            pub priority_tip: u64,
            /// Compute-unit limit the cranker emits as `SetComputeUnitLimit`
            /// right before `Trigger`. `0` = no ix (inherits the 200 k/ix
            /// default). Capped at `MAX_COMPUTE_UNIT_LIMIT` (1.4 M) at `Create`.
            pub cu_limit: u32,
            /// The scheduled instructions, in execution order. Must be non-empty
            pub scheduled: &'a [ScheduledIx<'a>],
        }

        /// Build a `Create` instruction scheduling a single instruction.
        pub fn create(payer: Pubkey, crank: Pubkey, args: &CreateArgs<'_>) -> Instruction {
            let body_len: usize = args
                .scheduled
                .iter()
                .map(|s| CREATE_IX_HEADER_LEN + 33 * s.metas.len() + s.data.len())
                .sum();
            let mut data = Vec::with_capacity(1 + CREATE_FIXED_PREFIX_LEN + body_len);
            data.push(ix::CREATE);
            data.extend_from_slice(&args.seed);
            data.extend_from_slice(&args.authority);
            data.extend_from_slice(&args.start_slot.to_le_bytes());
            data.extend_from_slice(&args.interval_slots.to_le_bytes());
            data.extend_from_slice(&args.remaining.to_le_bytes());
            data.extend_from_slice(&args.priority_tip.to_le_bytes());
            data.extend_from_slice(&args.cu_limit.to_le_bytes());
            for s in args.scheduled {
                data.push(s.metas.len() as u8);
                data.extend_from_slice(&(s.data.len() as u16).to_le_bytes());
                data.extend_from_slice(&s.program_id.to_bytes());
                for m in s.metas {
                    let flag: u8 = if m.is_writable { META_FLAG_WRITABLE } else { 0 };
                    data.push(flag);
                    data.extend_from_slice(&m.pubkey.to_bytes());
                }
                data.extend_from_slice(s.data);
            }

            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new(payer, true),
                    AccountMeta::new(crank, false),
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
                ],
                data,
            }
        }

        /// Build a `Trigger` instruction. Must be paired in the same tx with the
        /// scheduled instruction at `current_ix_index + 1`.
        pub fn trigger(crank: Pubkey, cranker: Pubkey) -> Instruction {
            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new(crank, false),
                    AccountMeta::new(cranker, true),
                    AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
                ],
                data: alloc::vec![ix::TRIGGER],
            }
        }

        /// Build a `Cancel` instruction.
        pub fn cancel(authority: Pubkey, crank: Pubkey, recipient: Pubkey) -> Instruction {
            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new_readonly(authority, true),
                    AccountMeta::new(crank, false),
                    AccountMeta::new(recipient, false),
                ],
                data: alloc::vec![ix::CANCEL],
            }
        }

        /// Build a `Close` instruction (permissionless cleanup).
        pub fn close(reporter: Pubkey, crank: Pubkey, recipient: Pubkey) -> Instruction {
            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new(reporter, true),
                    AccountMeta::new(crank, false),
                    AccountMeta::new(recipient, false),
                ],
                data: alloc::vec![ix::CLOSE],
            }
        }
    }

    /// Builders targeting the ephemeral-rollup Hydra program
    /// ([`EPHEMERAL_PROGRAM_ID`]).
    pub mod ephemeral {
        use ephemeral_rollups_pinocchio::consts::{EPHEMERAL_VAULT_ID, MAGIC_PROGRAM_ID};

        use super::*;

        /// This module's program ID.
        pub const PROGRAM_ID: Pubkey = super::EPHEMERAL_PROGRAM_ID;

        /// Derive `(crank_pda, bump)` under the ephemeral program.
        pub fn find_crank_pda(seed: &[u8; 32]) -> (Pubkey, u8) {
            Pubkey::find_program_address(&[crate::consts::CRANK_SEED_PREFIX, seed], &PROGRAM_ID)
        }

        /// All the scheduling knobs for `CreateEphemeral`
        pub struct CreateArgs<'a> {
            pub seed: [u8; 32],
            /// All-zeros = unkillable (no cancel authority).
            pub authority: [u8; 32],
            pub start_slots: u64,
            pub interval_slots: u64,
            /// `0` on the wire means "infinite"; Hydra stores `u64::MAX` internally.
            pub remaining: u64,
            pub priority_tip: u64,
            /// Compute-unit limit the cranker emits as `SetComputeUnitLimit`
            /// right before `Trigger`. `0` = no ix (inherits the 200 k/ix
            /// default). Capped at `MAX_COMPUTE_UNIT_LIMIT` (1.4 M) at `Create`.
            pub cu_limit: u32,
            /// The scheduled instructions, in execution order. Must be non-empty
            pub scheduled: &'a [ScheduledIx<'a>],
        }

        /// Build a `Create` instruction
        pub fn create(sponsor: Pubkey, crank: Pubkey, args: &CreateArgs<'_>) -> Instruction {
            let body_len: usize = args
                .scheduled
                .iter()
                .map(|s| super::CREATE_IX_HEADER_LEN + 33 * s.metas.len() + s.data.len())
                .sum();
            let mut data = Vec::with_capacity(1 + super::CREATE_FIXED_PREFIX_LEN + body_len);
            data.push(ix::CREATE);
            data.extend_from_slice(&args.seed);
            data.extend_from_slice(&args.authority);
            data.extend_from_slice(&args.start_slots.to_le_bytes());
            data.extend_from_slice(&args.interval_slots.to_le_bytes());
            data.extend_from_slice(&args.remaining.to_le_bytes());
            data.extend_from_slice(&args.priority_tip.to_le_bytes());
            data.extend_from_slice(&args.cu_limit.to_le_bytes());
            for s in args.scheduled {
                data.push(s.metas.len() as u8);
                data.extend_from_slice(&(s.data.len() as u16).to_le_bytes());
                data.extend_from_slice(&s.program_id.to_bytes());
                for m in s.metas {
                    let flag: u8 = if m.is_writable { META_FLAG_WRITABLE } else { 0 };
                    data.push(flag);
                    data.extend_from_slice(&m.pubkey.to_bytes());
                }
                data.extend_from_slice(s.data);
            }
            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new(sponsor, true),
                    AccountMeta::new(crank, false),
                    AccountMeta::new(EPHEMERAL_VAULT_ID, false),
                    AccountMeta::new_readonly(MAGIC_PROGRAM_ID, false),
                ],
                data,
            }
        }

        /// Build a `Trigger` instruction.
        pub fn trigger(crank: Pubkey, cranker: Pubkey) -> Instruction {
            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new(crank, false),
                    AccountMeta::new(cranker, true),
                    AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
                ],
                data: alloc::vec![ix::TRIGGER],
            }
        }

        /// Build a `Cancel` instruction
        pub fn cancel(authority: Pubkey, crank: Pubkey) -> Instruction {
            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new(authority, true),
                    AccountMeta::new(crank, false),
                    AccountMeta::new(EPHEMERAL_VAULT_ID, false),
                    AccountMeta::new_readonly(MAGIC_PROGRAM_ID, false),
                ],
                data: alloc::vec![ix::CANCEL],
            }
        }

        /// Build a `Close` instruction
        pub fn close(reporter: Pubkey, crank: Pubkey) -> Instruction {
            Instruction {
                program_id: PROGRAM_ID,
                accounts: alloc::vec![
                    AccountMeta::new(reporter, true),
                    AccountMeta::new(crank, false),
                    AccountMeta::new(EPHEMERAL_VAULT_ID, false),
                    AccountMeta::new_readonly(MAGIC_PROGRAM_ID, false),
                ],
                data: alloc::vec![ix::CLOSE],
            }
        }
    }

    #[cfg(not(feature = "ephemeral"))]
    pub use base::*;
    #[cfg(feature = "ephemeral")]
    pub use ephemeral::*;

    /// Reconstruct all scheduled instructions from a crank's raw account bytes.
    /// This is what an off-chain cranker does to build the sibling ixs that
    /// must follow `Trigger`.
    pub fn scheduled_ixs_from_crank(data: &[u8]) -> Option<Vec<Instruction>> {
        use crate::consts::{CRANK_HEADER_SIZE, SERIALIZED_META_SIZE};

        if data.len() < CRANK_HEADER_SIZE {
            return None;
        }
        let tail = &data[CRANK_HEADER_SIZE..];
        let mut out = Vec::new();
        let mut off = 0usize;
        while off < tail.len() {
            // [num_accounts: u16][metas: 33*N][program_id: 32][data_len: u16][data]
            if off + 2 > tail.len() {
                return None;
            }
            let num_accounts = u16::from_le_bytes(tail[off..off + 2].try_into().ok()?) as usize;
            let metas_start = off + 2;
            let metas_end = metas_start + num_accounts * SERIALIZED_META_SIZE;
            if metas_end + 32 + 2 > tail.len() {
                return None;
            }
            let program_id =
                Pubkey::new_from_array(tail[metas_end..metas_end + 32].try_into().ok()?);
            let data_len =
                u16::from_le_bytes(tail[metas_end + 32..metas_end + 34].try_into().ok()?) as usize;
            let data_start = metas_end + 34;
            let data_end = data_start + data_len;
            if data_end > tail.len() {
                return None;
            }

            let mut accounts = Vec::with_capacity(num_accounts);
            for i in 0..num_accounts {
                let base = metas_start + i * SERIALIZED_META_SIZE;
                let flag = tail[base];
                let pk = Pubkey::new_from_array(tail[base + 1..base + 33].try_into().ok()?);
                let is_writable = flag & META_FLAG_WRITABLE != 0;
                accounts.push(if is_writable {
                    AccountMeta::new(pk, false)
                } else {
                    AccountMeta::new_readonly(pk, false)
                });
            }

            out.push(Instruction {
                program_id,
                accounts,
                data: tail[data_start..data_end].to_vec(),
            });
            off = data_end;
        }

        if out.is_empty() {
            return None;
        }
        Some(out)
    }
}

#[cfg(feature = "client")]
pub use client::*;
