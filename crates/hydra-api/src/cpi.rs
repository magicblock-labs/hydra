//! On-chain CPI wrappers for integrators who want to invoke Hydra from
//! their own program.
//!
//! `native` is for `solana-program` / Anchor callers.
//! `pinocchio` is for Pinocchio callers.
//!
//! `Trigger` is not exposed here. It must be sent as a top-level instruction.

pub mod base {
    #[cfg(feature = "cpi-native")]
    pub mod native {
        //! CPI wrappers for `solana-program` / Anchor callers.
        //!
        //! # Example
        //!
        //! ```ignore
        //! use hydra_api::cpi::native as hydra_cpi;
        //! use hydra_api::instruction::{CreateArgs, SchedMeta};
        //!
        //! // Inside your user-facing instruction's handler:
        //! hydra_cpi::create(
        //!     payer_ai, crank_ai, system_program_ai,
        //!     &CreateArgs { seed, authority: [0u8; 32], /* ... */ },
        //! )?;
        //! ```

        use solana_account_info::AccountInfo;
        use solana_cpi::invoke_signed;
        use solana_program_error::ProgramError;

        use crate::instruction::{base as builder, CreateArgs};

        #[inline]
        pub fn create<'a>(
            payer: &AccountInfo<'a>,
            crank: &AccountInfo<'a>,
            system_program: &AccountInfo<'a>,
            args: &CreateArgs<'_>,
            signer_seeds: &[&[&[u8]]],
        ) -> Result<(), ProgramError> {
            let ix = builder::create(*payer.key, *crank.key, args);
            invoke_signed(
                &ix,
                &[payer.clone(), crank.clone(), system_program.clone()],
                signer_seeds,
            )
        }

        /// `signer_seeds` is typically `&[&[b"authority_seed", &[bump]]]` when
        /// `authority` is a PDA controlled by the integrator program, or
        /// `&[]` when it's an EOA.
        #[inline]
        pub fn cancel<'a>(
            authority: &AccountInfo<'a>,
            crank: &AccountInfo<'a>,
            recipient: &AccountInfo<'a>,
            signer_seeds: &[&[&[u8]]],
        ) -> Result<(), ProgramError> {
            let ix = builder::cancel(*authority.key, *crank.key, *recipient.key);
            invoke_signed(
                &ix,
                &[authority.clone(), crank.clone(), recipient.clone()],
                signer_seeds,
            )
        }

        /// `signer_seeds` is typically `&[]` unless the reporter is a PDA
        /// owned by the integrator program.
        #[inline]
        pub fn close<'a>(
            reporter: &AccountInfo<'a>,
            crank: &AccountInfo<'a>,
            recipient: &AccountInfo<'a>,
            signer_seeds: &[&[&[u8]]],
        ) -> Result<(), ProgramError> {
            let ix = builder::close(*reporter.key, *crank.key, *recipient.key);
            invoke_signed(
                &ix,
                &[reporter.clone(), crank.clone(), recipient.clone()],
                signer_seeds,
            )
        }
    }

    #[cfg(feature = "cpi-pinocchio")]
    pub mod pinocchio {
        //! CPI wrappers for Pinocchio callers.
        //!
        //! Build `Create` manually. See `examples/pinocchio/`.

        use pinocchio::{
            cpi::{invoke_signed, Signer},
            instruction::{InstructionAccount, InstructionView},
            AccountView, ProgramResult,
        };
        use solana_program_error::ProgramError;

        use crate::instruction as builder;
        use crate::{consts::ix as disc, instruction::CREATE_FIXED_PREFIX_LEN};

        #[inline]
        pub fn create<const N: usize>(
            payer: &AccountView,
            crank: &AccountView,
            system_program: &AccountView,
            args: &builder::CreateArgs<'_>,
            signers: &[Signer],
        ) -> ProgramResult {
            if 1 + CREATE_FIXED_PREFIX_LEN + args.body_len() > N {
                return Err(ProgramError::InvalidInstructionData);
            }

            let mut data = [0_u8; N];
            args.write_to(&mut data);

            let ix = InstructionView {
                program_id: &crate::base::ID,
                data: &data,
                accounts: &[
                    InstructionAccount::writable(payer.address()),
                    InstructionAccount::writable(crank.address()),
                    InstructionAccount::writable(system_program.address()),
                ],
            };

            invoke_signed(&ix, &[payer, crank, system_program], signers)
        }

        #[inline(always)]
        pub fn cancel(
            authority: &AccountView,
            crank: &AccountView,
            recipient: &AccountView,
            signers: &[Signer],
        ) -> ProgramResult {
            let data = [disc::CANCEL];
            let metas = [
                InstructionAccount::readonly_signer(authority.address()),
                InstructionAccount::writable(crank.address()),
                InstructionAccount::writable(recipient.address()),
            ];
            let ix = InstructionView {
                program_id: &crate::base::ID,
                accounts: &metas,
                data: &data,
            };
            invoke_signed(&ix, &[authority, crank, recipient], signers)
        }

        #[inline(always)]
        pub fn close(
            reporter: &AccountView,
            crank: &AccountView,
            recipient: &AccountView,
            signers: &[Signer],
        ) -> ProgramResult {
            let data = [disc::CLOSE];
            let metas = [
                InstructionAccount::writable_signer(reporter.address()),
                InstructionAccount::writable(crank.address()),
                InstructionAccount::writable(recipient.address()),
            ];
            let ix = InstructionView {
                program_id: &crate::base::ID,
                accounts: &metas,
                data: &data,
            };
            invoke_signed(&ix, &[reporter, crank, recipient], signers)
        }
    }
}

pub mod ephemeral {
    #[cfg(feature = "cpi-native")]
    pub mod native {
        //! CPI wrappers for `solana-program` / Anchor callers.
        //!
        //! # Example
        //!
        //! ```ignore
        //! use hydra_api::cpi::ephemeral::native as hydra_cpi;
        //! use hydra_api::instruction::{CreateArgs, SchedMeta};
        //!
        //! // Inside your user-facing instruction's handler:
        //! hydra_cpi::create(
        //!     sponsor_ai, crank_ai, vault_ai, magic_program_ai,
        //!     &CreateArgs { seed, authority: [0u8; 32], /* ... */ },
        //! )?;
        //! ```

        use solana_account_info::AccountInfo;
        use solana_cpi::invoke_signed;
        use solana_program_error::ProgramError;

        use crate::instruction::{ephemeral as builder, CreateArgs};

        #[inline]
        pub fn create<'a>(
            sponsor: &AccountInfo<'a>,
            crank: &AccountInfo<'a>,
            vault: &AccountInfo<'a>,
            magic_program: &AccountInfo<'a>,
            args: &CreateArgs<'_>,
            signer_seeds: &[&[&[u8]]],
        ) -> Result<(), ProgramError> {
            let ix = builder::create(*sponsor.key, *crank.key, args);
            invoke_signed(
                &ix,
                &[
                    sponsor.clone(),
                    crank.clone(),
                    vault.clone(),
                    magic_program.clone(),
                ],
                signer_seeds,
            )
        }

        /// `signer_seeds` is typically `&[&[b"authority_seed", &[bump]]]` when
        /// `authority` is a PDA controlled by the integrator program, or
        /// `&[]` when it's an EOA.
        #[inline]
        pub fn cancel<'a>(
            authority: &AccountInfo<'a>,
            crank: &AccountInfo<'a>,
            recipient: &AccountInfo<'a>,
            vault: &AccountInfo<'a>,
            magic_program: &AccountInfo<'a>,
            signer_seeds: &[&[&[u8]]],
        ) -> Result<(), ProgramError> {
            let ix = builder::cancel(*authority.key, *crank.key, *recipient.key);
            invoke_signed(
                &ix,
                &[
                    authority.clone(),
                    crank.clone(),
                    recipient.clone(),
                    vault.clone(),
                    magic_program.clone(),
                ],
                signer_seeds,
            )
        }

        /// `signer_seeds` is typically `&[]` unless the reporter is a PDA
        /// owned by the integrator program.
        #[inline]
        pub fn close<'a>(
            reporter: &AccountInfo<'a>,
            crank: &AccountInfo<'a>,
            recipient: &AccountInfo<'a>,
            vault: &AccountInfo<'a>,
            magic_program: &AccountInfo<'a>,
            signer_seeds: &[&[&[u8]]],
        ) -> Result<(), ProgramError> {
            let ix = builder::close(*reporter.key, *crank.key, *recipient.key);
            invoke_signed(
                &ix,
                &[
                    reporter.clone(),
                    crank.clone(),
                    recipient.clone(),
                    vault.clone(),
                    magic_program.clone(),
                ],
                signer_seeds,
            )
        }
    }

    #[cfg(feature = "cpi-pinocchio")]
    pub mod pinocchio {
        //! CPI wrappers for Pinocchio callers.
        //!
        //! Build `Create` manually. See `examples/pinocchio/`.

        use pinocchio::{
            cpi::{invoke_signed, Signer},
            instruction::{InstructionAccount, InstructionView},
            AccountView, ProgramResult,
        };
        use solana_program_error::ProgramError;

        use crate::{
            consts::ix as disc,
            instruction::{CreateArgs, CREATE_FIXED_PREFIX_LEN},
        };

        #[inline]
        pub fn create<const N: usize>(
            sponsor: &AccountView,
            crank: &AccountView,
            vault: &AccountView,
            magic_program: &AccountView,
            args: &CreateArgs<'_>,
            signers: &[Signer],
        ) -> ProgramResult {
            if 1 + CREATE_FIXED_PREFIX_LEN + args.body_len() > N {
                return Err(ProgramError::InvalidInstructionData);
            }

            let mut data = [0_u8; N];
            args.write_to(&mut data);

            let ix = InstructionView {
                program_id: &crate::ephemeral::ID,
                data: &data,
                accounts: &[
                    InstructionAccount::writable_signer(sponsor.address()),
                    InstructionAccount::writable(crank.address()),
                    InstructionAccount::writable(vault.address()),
                    InstructionAccount::readonly(magic_program.address()),
                ],
            };
            invoke_signed(&ix, &[sponsor, crank, vault, magic_program], signers)
        }

        #[inline(always)]
        pub fn cancel(
            authority: &AccountView,
            crank: &AccountView,
            recipient: &AccountView,
            vault: &AccountView,
            magic_program: &AccountView,
            signers: &[Signer],
        ) -> ProgramResult {
            let data = [disc::CANCEL];
            let metas = [
                InstructionAccount::writable_signer(authority.address()),
                InstructionAccount::writable(crank.address()),
                InstructionAccount::writable(recipient.address()),
                InstructionAccount::writable(vault.address()),
                InstructionAccount::readonly(magic_program.address()),
            ];
            let ix = InstructionView {
                program_id: &crate::ephemeral::ID,
                accounts: &metas,
                data: &data,
            };
            invoke_signed(
                &ix,
                &[authority, crank, recipient, vault, magic_program],
                signers,
            )
        }

        #[inline(always)]
        pub fn close(
            reporter: &AccountView,
            crank: &AccountView,
            recipient: &AccountView,
            vault: &AccountView,
            magic_program: &AccountView,
            signers: &[Signer],
        ) -> ProgramResult {
            let data = [disc::CLOSE];
            let metas = [
                InstructionAccount::writable_signer(reporter.address()),
                InstructionAccount::writable(crank.address()),
                InstructionAccount::writable(recipient.address()),
                InstructionAccount::writable(vault.address()),
                InstructionAccount::readonly(magic_program.address()),
            ];
            let ix = InstructionView {
                program_id: &crate::ephemeral::ID,
                accounts: &metas,
                data: &data,
            };
            invoke_signed(
                &ix,
                &[reporter, crank, recipient, vault, magic_program],
                signers,
            )
        }
    }
}
