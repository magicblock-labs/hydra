//! Shared helpers for the ephemeral-rollup crank processors.

use ephemeral_rollups_pinocchio::consts::{EPHEMERAL_VAULT_ID, MAGIC_PROGRAM_ID};
use pinocchio::{error::ProgramError, AccountView, ProgramResult};

/// Reject calls that don't reference the real Magic program + ephemeral vault,
/// so the CPI can't be pointed at an impostor program/vault.
#[inline(always)]
pub(super) fn check_magic_accounts(
    vault: &AccountView,
    magic_program: &AccountView,
) -> ProgramResult {
    if vault.address() != &EPHEMERAL_VAULT_ID || magic_program.address() != &MAGIC_PROGRAM_ID {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}
