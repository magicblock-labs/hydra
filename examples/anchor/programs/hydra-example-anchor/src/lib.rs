//! Minimal Anchor integrator. Demonstrates that `hydra-api`'s `cpi-native`
//! feature works unchanged from inside an Anchor program — Anchor uses
//! `solana-program` under the hood, so the same `AccountInfo`s flow through.
//!
//! The only gotcha vs. the plain-native example: Anchor's `Context<Accounts>`
//! wraps each account, so you call `.to_account_info()` when handing them
//! to `hydra_cpi::create`.

use anchor_lang::prelude::*;

use hydra_api::{cpi::native as hydra_cpi, instruction::CreateArgs};

declare_id!("Xyj597GykzwSu44muqNHtYs2aKUgm9ydNoHNySTDFs5");

#[program]
pub mod hydra_example_anchor {
    use super::*;

    pub fn schedule(
        ctx: Context<Schedule>,
        seed: [u8; 32],
        target_program_id: Pubkey,
    ) -> Result<()> {
        hydra_cpi::create(
            &ctx.accounts.payer.to_account_info(),
            &ctx.accounts.crank.to_account_info(),
            &ctx.accounts.system_program.to_account_info(),
            &CreateArgs {
                seed,
                authority: [0u8; 32],
                start_slot: 0,
                interval_slots: 400,
                remaining: 10,
                priority_tip: 1_000,
                cu_limit: 0, // no on-chain CU override
                scheduled_program_id: target_program_id,
                scheduled_metas: &[],
                scheduled_data: b"tick",
            },
        )
        .map_err(Into::into)
    }
}

#[derive(Accounts)]
pub struct Schedule<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: Hydra validates this PDA matches `[b"crank", seed]` at Create.
    #[account(mut)]
    pub crank: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}
