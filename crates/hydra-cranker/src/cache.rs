//! In-memory index of on-chain cranks. Populated at bootstrap by
//! `getProgramAccounts` and kept fresh by `programSubscribe` notifications
//! in [`crate::watch`]. The [`Cache`] itself is `Arc<Mutex<HashMap<..>>>`
//! — both the WS thread and the trigger loop hold a clone.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use solana_pubkey::Pubkey;

use hydra_api::consts::{CRANKER_REWARD, CRANK_HEADER_SIZE, STALENESS_THRESHOLD_SLOTS};

/// Minimal decoded projection of a Crank account — just the fields we need
/// for eligibility checks. The full raw bytes live in `data` so the trigger
/// loop can rebuild the scheduled instruction.
#[derive(Clone)]
pub struct CrankEntry {
    pub pubkey: Pubkey,
    pub lamports: u64,
    /// `[0; 32]` = no cancel authority; `Close` is then free to pick any
    /// recipient. Non-zero = `Close` must refund the remainder to this pubkey.
    pub authority: [u8; 32],
    pub next_exec_slot: u64,
    pub remaining: u64,
    pub priority_tip: u64,
    pub rent_min: u64,
    /// `0` = cranker omits `SetComputeUnitLimit`.
    pub cu_limit: u32,
    pub data: Vec<u8>,
}

impl CrankEntry {
    /// Decode the header offsets from the raw account bytes. Returns `None`
    /// if the buffer is too small or malformed.
    pub fn from_raw(pubkey: Pubkey, lamports: u64, data: &[u8]) -> Option<Self> {
        if data.len() < CRANK_HEADER_SIZE {
            return None;
        }
        let authority: [u8; 32] = data[0..32].try_into().ok()?;
        let next_exec_slot = u64::from_le_bytes(data[64..72].try_into().ok()?);
        let remaining = u64::from_le_bytes(data[80..88].try_into().ok()?);
        let priority_tip = u64::from_le_bytes(data[88..96].try_into().ok()?);
        let rent_min = u64::from_le_bytes(data[104..112].try_into().ok()?);
        let cu_limit = u32::from_le_bytes(data[115..119].try_into().ok()?);
        Some(Self {
            pubkey,
            lamports,
            authority,
            next_exec_slot,
            remaining,
            priority_tip,
            rent_min,
            cu_limit,
            data: data.to_vec(),
        })
    }

    /// Mirrors Hydra's on-chain Trigger pre-flight: slot reached, not
    /// exhausted, enough lamports to cover reward + tip above the rent floor.
    pub fn is_eligible(&self, current_slot: u64) -> bool {
        if current_slot < self.next_exec_slot {
            return false;
        }
        if self.remaining == 0 {
            return false;
        }
        let reward = CRANKER_REWARD.saturating_add(self.priority_tip);
        self.lamports >= self.rent_min.saturating_add(reward)
    }

    /// Mirrors on-chain `Close` pre-condition: exhausted OR underfunded OR
    /// stuck (`current_slot - next_exec_slot > STALENESS_THRESHOLD_SLOTS`).
    pub fn is_closable(&self, current_slot: u64) -> bool {
        if self.remaining == 0 {
            return true;
        }
        let next_reward = CRANKER_REWARD.saturating_add(self.priority_tip);
        if self.lamports < self.rent_min.saturating_add(next_reward) {
            return true;
        }
        // `saturating_sub` keeps future-scheduled cranks trivially not stale.
        current_slot.saturating_sub(self.next_exec_slot) > STALENESS_THRESHOLD_SLOTS
    }
}

/// Shared cache handle. Cheap to clone.
pub type Cache = Arc<Mutex<HashMap<Pubkey, CrankEntry>>>;

pub fn new_cache() -> Cache {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Outcome of a cache mutation, used by callers to drive metrics labels.
pub enum CacheOutcome {
    Inserted,
    Updated,
    Removed,
    Unchanged,
}

/// Apply a single account update to the cache. Removes entries that have
/// been closed (zero lamports / empty data) or are no longer well-formed
/// Crank accounts; otherwise inserts/updates the decoded entry.
///
/// Shared between the WS `programSubscribe` path and the gRPC stream so
/// the two stay byte-for-byte consistent.
pub fn apply_update(cache: &Cache, pubkey: Pubkey, lamports: u64, data: &[u8]) -> CacheOutcome {
    let mut guard = cache.lock().expect("cache poisoned");
    if lamports == 0 || data.is_empty() {
        return if guard.remove(&pubkey).is_some() {
            CacheOutcome::Removed
        } else {
            CacheOutcome::Unchanged
        };
    }
    match CrankEntry::from_raw(pubkey, lamports, data) {
        Some(e) => {
            if guard.insert(pubkey, e).is_some() {
                CacheOutcome::Updated
            } else {
                CacheOutcome::Inserted
            }
        }
        None => {
            if guard.remove(&pubkey).is_some() {
                CacheOutcome::Removed
            } else {
                CacheOutcome::Unchanged
            }
        }
    }
}
