//! Instruction wire layouts + (behind `client` feature) host-side builders.
//!
//! # `Create` (disc 0) — wire layout
//!
//! ```text
//! seed:           [u8; 32]
//! authority:      [u8; 32]      // all-zeros = none
//! start_slot:     u64 LE
//! interval_slots: u64 LE
//! remaining:      u64 LE        // 0 on the wire = infinite
//! priority_tip:   u64 LE
//! cu_limit:       u32 LE        // 0 = cranker omits SetComputeUnitLimit
//! num_accounts:   u8
//! data_len:       u16 LE
//! program_id:     [u8; 32]
//! metas:          [[flag:u8][pubkey:[u8;32]]; num_accounts]
//! data:           [u8; data_len]
//! ```
//!
//! # `Trigger` (disc 1)   Accounts: `[crank(w), cranker(w,s), instructions_sysvar]`
//! # `Cancel`  (disc 2)   Accounts: `[authority(s), crank(w), recipient(w)]`
//! # `Close`   (disc 3)   Accounts: `[reporter(s,w), crank(w), recipient(w)]`
//!
//! To fund a crank after creation, send a direct `system_program::transfer`
//! to the crank PDA — no dedicated instruction is needed.

/// Fixed-size prefix of `Create` data before the variable metas + data section.
pub const CREATE_FIXED_PREFIX_LEN: usize = 32 +  // seed
    32 +  // authority
     8 +  // start_slot
     8 +  // interval_slots
     8 +  // remaining
     8 +  // priority_tip
     4 +  // cu_limit
     1 +  // num_accounts
     2 +  // data_len
    32; // program_id
        // = 135

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

    /// Solana's built-in instructions sysvar pubkey.
    pub const INSTRUCTIONS_SYSVAR_ID: Pubkey =
        pubkey!("Sysvar1nstructions1111111111111111111111111");

    /// Solana's system program pubkey.
    pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");

    /// Hydra program ID as a `solana_pubkey::Pubkey` (convenience for clients).
    pub fn program_id() -> Pubkey {
        Pubkey::new_from_array(crate::ID.to_bytes())
    }

    /// Derive `(crank_pda, bump)` using `solana_pubkey::Pubkey`.
    pub fn find_crank_pda(seed: &[u8; 32]) -> (Pubkey, u8) {
        let (addr, bump) = crate::state::find_crank_pda(seed);
        (Pubkey::new_from_array(addr.to_bytes()), bump)
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

    /// All the scheduling knobs for `Create`.
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
        pub scheduled_program_id: Pubkey,
        pub scheduled_metas: &'a [SchedMeta],
        pub scheduled_data: &'a [u8],
    }

    /// Build a `Create` instruction.
    pub fn create(payer: Pubkey, crank: Pubkey, args: &CreateArgs<'_>) -> Instruction {
        let mut data = Vec::with_capacity(
            1 + super::CREATE_FIXED_PREFIX_LEN
                + 33 * args.scheduled_metas.len()
                + args.scheduled_data.len(),
        );
        data.push(ix::CREATE);
        data.extend_from_slice(&args.seed);
        data.extend_from_slice(&args.authority);
        data.extend_from_slice(&args.start_slot.to_le_bytes());
        data.extend_from_slice(&args.interval_slots.to_le_bytes());
        data.extend_from_slice(&args.remaining.to_le_bytes());
        data.extend_from_slice(&args.priority_tip.to_le_bytes());
        data.extend_from_slice(&args.cu_limit.to_le_bytes());
        data.push(args.scheduled_metas.len() as u8);
        data.extend_from_slice(&(args.scheduled_data.len() as u16).to_le_bytes());
        data.extend_from_slice(&args.scheduled_program_id.to_bytes());
        for m in args.scheduled_metas {
            let flag: u8 = if m.is_writable { META_FLAG_WRITABLE } else { 0 };
            data.push(flag);
            data.extend_from_slice(&m.pubkey.to_bytes());
        }
        data.extend_from_slice(args.scheduled_data);

        Instruction {
            program_id: program_id(),
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
            program_id: program_id(),
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
            program_id: program_id(),
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
            program_id: program_id(),
            accounts: alloc::vec![
                AccountMeta::new(reporter, true),
                AccountMeta::new(crank, false),
                AccountMeta::new(recipient, false),
            ],
            data: alloc::vec![ix::CLOSE],
        }
    }

    /// Reconstruct the scheduled instruction from a crank's raw account bytes.
    /// This is what an off-chain cranker does to build `ix[k+1]` for the tx.
    pub fn scheduled_ix_from_crank(data: &[u8]) -> Option<Instruction> {
        use crate::consts::{CRANK_HEADER_SIZE, SERIALIZED_META_SIZE};

        if data.len() < CRANK_HEADER_SIZE + 2 {
            return None;
        }
        // Tail region starts right after the 120-byte header and is laid out
        // in the instructions-sysvar wire format.
        let tail = &data[CRANK_HEADER_SIZE..];
        let num_accounts = u16::from_le_bytes(tail[0..2].try_into().ok()?) as usize;
        let metas_end = 2 + num_accounts * SERIALIZED_META_SIZE;
        if tail.len() < metas_end + 32 + 2 {
            return None;
        }
        let program_id = Pubkey::new_from_array(tail[metas_end..metas_end + 32].try_into().ok()?);
        let data_len =
            u16::from_le_bytes(tail[metas_end + 32..metas_end + 34].try_into().ok()?) as usize;
        let data_start = metas_end + 34;
        if tail.len() < data_start + data_len {
            return None;
        }

        let mut accounts = Vec::with_capacity(num_accounts);
        for i in 0..num_accounts {
            let base = 2 + i * SERIALIZED_META_SIZE;
            let flag = tail[base];
            let pk = Pubkey::new_from_array(tail[base + 1..base + 33].try_into().ok()?);
            let is_writable = flag & META_FLAG_WRITABLE != 0;
            accounts.push(if is_writable {
                AccountMeta::new(pk, false)
            } else {
                AccountMeta::new_readonly(pk, false)
            });
        }

        Some(Instruction {
            program_id,
            accounts,
            data: tail[data_start..data_start + data_len].to_vec(),
        })
    }
}

#[cfg(feature = "client")]
pub use client::*;
