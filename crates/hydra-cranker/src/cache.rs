//! In-memory index of on-chain cranks. Populated at bootstrap by
//! `getProgramAccounts` and kept fresh by `programSubscribe` notifications
//! in [`crate::watch`]. The [`Cache`] itself is `Arc<Mutex<HashMap<..>>>`
//! — both the WS thread and the trigger loop hold a clone.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use solana_pubkey::Pubkey;

use hydra_api::consts::{
    CRANKER_REWARD, CRANK_HEADER_SIZE, SERIALIZED_META_SIZE, STALENESS_THRESHOLD_SLOTS,
};

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

    /// True if any scheduled instruction lists `account` among its metas,
    /// regardless of the stored read/write flag. Such a crank is unsafe to run
    /// when `account` is the cranker's own pubkey: as the tx fee payer the
    /// cranker is signer + writable, so the runtime promotes every reference to
    /// it and the crank can never fire (and must not, since it would grant a
    /// scheduled ix write access to the cranker's account).
    ///
    /// Scans the stored tail bytes in place.
    pub fn references_account(&self, account: &Pubkey) -> bool {
        let target: &[u8] = account.as_ref();
        let Some(tail) = self.data.get(CRANK_HEADER_SIZE..) else {
            return false;
        };
        // Tail blobs, back-to-back:
        // [num_accounts u16][ [flag u8][pk 32] * n ][program_id 32][data_len u16][data]
        let mut off = 0usize;
        while off < tail.len() {
            if off + 2 > tail.len() {
                return false;
            }
            let num_accounts = u16::from_le_bytes([tail[off], tail[off + 1]]) as usize;
            off += 2;
            let metas_len = num_accounts * SERIALIZED_META_SIZE;
            // Need the metas, the program id, and the data-len field.
            if off + metas_len + 32 + 2 > tail.len() {
                return false;
            }
            for i in 0..num_accounts {
                let pk_start = off + i * SERIALIZED_META_SIZE + 1; // skip flag byte
                if &tail[pk_start..pk_start + 32] == target {
                    return true;
                }
            }
            off += metas_len + 32;
            let data_len = u16::from_le_bytes([tail[off], tail[off + 1]]) as usize;
            off += 2;
            if off + data_len > tail.len() {
                return false;
            }
            off += data_len;
        }
        false
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

#[cfg(test)]
mod tests {
    use super::*;
    use hydra_api::consts::{CRANK_HEADER_SIZE, META_FLAG_WRITABLE};

    /// Build a raw crank buffer (120-byte header + one scheduled ix that lists
    /// `metas` in the on-chain tail wire layout).
    fn crank_with_metas(metas: &[(Pubkey, bool)]) -> Vec<u8> {
        let mut data = vec![0u8; CRANK_HEADER_SIZE];
        // tail: [num_accounts u16][ [flag u8][pk 32] * n ][program_id 32][data_len u16][data]
        data.extend_from_slice(&(metas.len() as u16).to_le_bytes());
        for (pk, writable) in metas {
            data.push(if *writable { META_FLAG_WRITABLE } else { 0 });
            data.extend_from_slice(pk.as_ref());
        }
        data.extend_from_slice(Pubkey::new_unique().as_ref()); // program_id
        data.extend_from_slice(&0u16.to_le_bytes()); // data_len = 0
        data
    }

    #[test]
    fn references_account_detects_metas_regardless_of_flag() {
        let cranker = Pubkey::new_unique();
        let other = Pubkey::new_unique();

        // Read-only reference is still a reference.
        let ro = CrankEntry::from_raw(
            Pubkey::new_unique(),
            1,
            &crank_with_metas(&[(cranker, false)]),
        )
        .unwrap();
        assert!(ro.references_account(&cranker));
        assert!(!ro.references_account(&other));

        // Writable reference too.
        let rw = CrankEntry::from_raw(
            Pubkey::new_unique(),
            1,
            &crank_with_metas(&[(cranker, true)]),
        )
        .unwrap();
        assert!(rw.references_account(&cranker));

        // A schedule that does not mention the cranker is safe.
        let clean =
            CrankEntry::from_raw(Pubkey::new_unique(), 1, &crank_with_metas(&[(other, true)]))
                .unwrap();
        assert!(!clean.references_account(&cranker));
    }

    /// One scheduled ix spec: its `(pubkey, is_writable)` metas plus its data.
    type IxSpec<'a> = (&'a [(Pubkey, bool)], &'a [u8]);

    /// Build a raw crank buffer holding several scheduled ixs back-to-back, each
    /// with its own metas + non-empty data. Exercises the scanner's blob-to-blob
    /// advance (num_accounts, metas, program_id, data_len, data).
    fn crank_with_ixs(ixs: &[IxSpec]) -> Vec<u8> {
        let mut data = vec![0u8; CRANK_HEADER_SIZE];
        for (metas, ix_data) in ixs {
            data.extend_from_slice(&(metas.len() as u16).to_le_bytes());
            for (pk, writable) in *metas {
                data.push(if *writable { META_FLAG_WRITABLE } else { 0 });
                data.extend_from_slice(pk.as_ref());
            }
            data.extend_from_slice(Pubkey::new_unique().as_ref()); // program_id
            data.extend_from_slice(&(ix_data.len() as u16).to_le_bytes());
            data.extend_from_slice(ix_data);
        }
        data
    }

    #[test]
    fn references_account_walks_across_multiple_ixs() {
        let target = Pubkey::new_unique();
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();

        // Target only appears in the *third* ix, after two blobs with data.
        let raw = crank_with_ixs(&[
            (&[(a, true)], b"first"),
            (&[(b, false)], b"second-longer"),
            (&[(a, false), (target, true)], b"third"),
        ]);
        let entry = CrankEntry::from_raw(Pubkey::new_unique(), 1, &raw).unwrap();
        assert!(entry.references_account(&target));
        assert!(entry.references_account(&a));
        assert!(entry.references_account(&b));
        assert!(!entry.references_account(&Pubkey::new_unique()));
    }

    #[test]
    fn references_account_false_on_malformed_tail() {
        // Header claims one scheduled account but the tail is truncated.
        let mut raw = vec![0u8; CRANK_HEADER_SIZE];
        raw.extend_from_slice(&1u16.to_le_bytes()); // num_accounts = 1
        raw.push(0); // flag, then the 32-byte pubkey is missing
        let entry = CrankEntry::from_raw(Pubkey::new_unique(), 1, &raw).unwrap();
        assert!(!entry.references_account(&Pubkey::new_unique()));
    }
}
