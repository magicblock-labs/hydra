//! Runtime selection of the target Hydra program.
//!
//! The cranker is a single binary that can drive either the base-layer program
//! or the ephemeral-rollup program; the choice is made once at startup from the
//! `--ephemeral` flag (not a compile-time feature). The base and ephemeral
//! cranks differ in their program ID, their `Close` account layout, and their
//! funding model (ephemeral cranks hold zero lamports), so the hot paths consult
//! this module rather than threading a flag through every call.

use std::sync::OnceLock;

use solana_pubkey::Pubkey;

use hydra_api::instruction as ix;

static EPHEMERAL: OnceLock<bool> = OnceLock::new();

/// Record the selected mode. Call once, before any watcher or the trigger loop
/// starts. Later calls are ignored.
pub fn init(ephemeral: bool) {
    let _ = EPHEMERAL.set(ephemeral);
}

/// Whether the cranker targets the ephemeral-rollup program.
pub fn is_ephemeral() -> bool {
    EPHEMERAL.get().copied().unwrap_or(false)
}

/// The program ID the cranker watches and submits to.
pub fn program_id() -> Pubkey {
    if is_ephemeral() {
        ix::EPHEMERAL_PROGRAM_ID
    } else {
        ix::BASE_PROGRAM_ID
    }
}
