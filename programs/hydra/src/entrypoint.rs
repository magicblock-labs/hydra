//! Program entrypoint and top-level discriminator dispatcher.

use pinocchio::error::ProgramError;
use pinocchio::{
    no_allocator, nostd_panic_handler, program_entrypoint, AccountView, Address, ProgramResult,
};

use hydra_api::{consts::ix, HydraError};

use crate::processor;

program_entrypoint!(process_instruction);
no_allocator!();
nostd_panic_handler!();

pub fn process_instruction(
    _program_id: &Address,
    accounts: &mut [AccountView],
    instruction_data: &[u8],
) -> ProgramResult {
    inner_process_instruction(accounts, instruction_data).inspect_err(log_error)
}

#[inline(never)]
fn inner_process_instruction(
    accounts: &mut [AccountView],
    instruction_data: &[u8],
) -> ProgramResult {
    let [discriminator, rest @ ..] = instruction_data else {
        return Err(HydraError::InvalidInstruction.into());
    };

    match *discriminator {
        ix::CREATE => {
            #[cfg(feature = "logging")]
            pinocchio_log::log!("ix: Create");
            processor::create::process(accounts, rest)
        }
        ix::TRIGGER => {
            #[cfg(feature = "logging")]
            pinocchio_log::log!("ix: Trigger");
            processor::trigger::process(accounts, rest)
        }
        ix::CANCEL => {
            #[cfg(feature = "logging")]
            pinocchio_log::log!("ix: Cancel");
            processor::cancel::process(accounts, rest)
        }
        ix::CLOSE => {
            #[cfg(feature = "logging")]
            pinocchio_log::log!("ix: Close");
            processor::close::process(accounts, rest)
        }
        _ => Err(HydraError::InvalidInstruction.into()),
    }
}

#[cold]
fn log_error(_error: &ProgramError) {
    // When `logging` is off, this is a no-op: zero CU on the error path,
    // and the raw `ProgramError` propagates to the runtime. When `logging`
    // is on, emit the variant's name (including `HydraError::*` for Custom
    // values) through the `sol_log_` syscall — no `format!`, no alloc.
    #[cfg(feature = "logging")]
    {
        // `to_str` is an inherent method on `ProgramError` that internally
        // calls `HydraError::to_str` (defined in hydra-api::error) for
        // `ProgramError::Custom(n)` values.
        let msg: &'static str = _error.to_str::<HydraError>();
        #[cfg(target_os = "solana")]
        // SAFETY: sol_log_ takes (ptr, len) of a byte buffer the syscall
        // reads read-only for the duration of the call.
        unsafe {
            solana_define_syscall::definitions::sol_log_(msg.as_ptr(), msg.len() as u64);
        }
    }
}
