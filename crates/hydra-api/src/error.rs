//! Custom errors. All map to `ProgramError::Custom(variant as u32)`.
//!
//! `ToStr` + `TryFrom<u32>` are implemented so the on-chain `log_error` path
//! (under the `logging` feature) can emit a named string via
//! `ProgramError::to_str::<HydraError>()`.

use pinocchio::error::{ProgramError, ToStr};

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HydraError {
    InvalidInstruction = 0,
    NotYetExecutable = 1,
    Exhausted = 2,
    /// The crank can't afford reward + priority tip while remaining rent-exempt.
    InsufficientFunds = 3,
    UnauthorizedAuthority = 4,
    /// `Close` pre-conditions unmet (not exhausted, not underfunded, not stale).
    NotClosable = 5,
    /// No follow-up instruction at `current_index + 1`.
    MissingFollowupInstruction = 6,
    /// Follow-up instruction's bytes do not match the stored template.
    MismatchedFollowupIx = 7,
    /// `Create` was given a meta with `is_signer = 1`.
    SignerInScheduledIx = 8,
    /// `Create` data was well-formed but semantically invalid (e.g.
    /// `remaining = 0 && interval_slots = 0`).
    InvalidSchedule = 9,
}

impl From<HydraError> for ProgramError {
    #[inline(always)]
    fn from(e: HydraError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

impl TryFrom<u32> for HydraError {
    type Error = ProgramError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::InvalidInstruction),
            1 => Ok(Self::NotYetExecutable),
            2 => Ok(Self::Exhausted),
            3 => Ok(Self::InsufficientFunds),
            4 => Ok(Self::UnauthorizedAuthority),
            5 => Ok(Self::NotClosable),
            6 => Ok(Self::MissingFollowupInstruction),
            7 => Ok(Self::MismatchedFollowupIx),
            8 => Ok(Self::SignerInScheduledIx),
            9 => Ok(Self::InvalidSchedule),
            _ => Err(ProgramError::InvalidArgument),
        }
    }
}

impl ToStr for HydraError {
    fn to_str(&self) -> &'static str {
        match self {
            Self::InvalidInstruction => "HydraError::InvalidInstruction",
            Self::NotYetExecutable => "HydraError::NotYetExecutable",
            Self::Exhausted => "HydraError::Exhausted",
            Self::InsufficientFunds => "HydraError::InsufficientFunds",
            Self::UnauthorizedAuthority => "HydraError::UnauthorizedAuthority",
            Self::NotClosable => "HydraError::NotClosable",
            Self::MissingFollowupInstruction => "HydraError::MissingFollowupInstruction",
            Self::MismatchedFollowupIx => "HydraError::MismatchedFollowupIx",
            Self::SignerInScheduledIx => "HydraError::SignerInScheduledIx",
            Self::InvalidSchedule => "HydraError::InvalidSchedule",
        }
    }
}
