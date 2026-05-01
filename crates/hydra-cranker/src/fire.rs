//! Build + submit crank transactions.

use anyhow::{anyhow, Result};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentLevel;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::{pubkey, Pubkey};
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_signer::Signer;
use solana_transaction::Transaction;

use hydra_api::instruction as ix;

use crate::cache::CrankEntry;
use crate::metrics;

const COMPUTE_BUDGET_ID: Pubkey = pubkey!("ComputeBudget111111111111111111111111111111");

/// `SetComputeUnitPrice(u64)` — discriminator 3, then price LE.
fn set_compute_unit_price(micro_lamports_per_cu: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3);
    data.extend_from_slice(&micro_lamports_per_cu.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_ID,
        accounts: Vec::new(),
        data,
    }
}

/// `SetComputeUnitLimit(u32)` — discriminator 2, then limit LE.
fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_ID,
        accounts: Vec::new(),
        data,
    }
}

pub fn fire_trigger(
    rpc: &RpcClient,
    cranker: &Keypair,
    entry: &CrankEntry,
    priority_fee_micro_lamports: u64,
    skip_preflight: bool,
) -> Result<()> {
    let scheduled = ix::scheduled_ix_from_crank(&entry.data)
        .ok_or_else(|| anyhow!("malformed crank tail for {}", entry.pubkey))?;
    let trigger = ix::trigger(entry.pubkey, cranker.pubkey());
    let blockhash = rpc.get_latest_blockhash().map_err(|e| {
        metrics::metrics()
            .rpc_errors_total
            .with_label_values(&["get_latest_blockhash"])
            .inc();
        anyhow::Error::new(e).context("latest_blockhash")
    })?;
    // `verify_followup` requires `scheduled` at `current_ix_index + 1`, so
    // it must sit immediately after `Trigger`; ComputeBudget ixs go before.
    let mut ixs: Vec<Instruction> = Vec::with_capacity(4);
    if entry.cu_limit > 0 {
        ixs.push(set_compute_unit_limit(entry.cu_limit));
    }
    if priority_fee_micro_lamports > 0 {
        ixs.push(set_compute_unit_price(priority_fee_micro_lamports));
    }
    ixs.push(trigger);
    ixs.push(scheduled);
    let msg = Message::new_with_blockhash(&ixs, Some(&cranker.pubkey()), &blockhash);
    let tx = Transaction::new(&[cranker], msg, blockhash);
    // Preflight catches reverts before the leader charges fees, but also
    // hides failures (no on-chain sig). Operators can opt into
    // `skip_preflight = true` to land failing txs on-chain for debugging.
    rpc.send_transaction_with_config(
        &tx,
        RpcSendTransactionConfig {
            skip_preflight,
            max_retries: Some(5),
            preflight_commitment: Some(CommitmentLevel::Processed),
            ..Default::default()
        },
    )
    .map_err(|e| {
        metrics::metrics()
            .rpc_errors_total
            .with_label_values(&["send_transaction"])
            .inc();
        anyhow::Error::new(e).context("send_transaction")
    })?;
    Ok(())
}

/// Submit a permissionless `Close`. The cranker keeps the `CRANKER_REWARD`
/// bounty; the remaining rent goes to `entry.authority` if set, otherwise to
/// the cranker (on-chain anti-grief check in close.rs).
pub fn fire_close(
    rpc: &RpcClient,
    cranker: &Keypair,
    entry: &CrankEntry,
    priority_fee_micro_lamports: u64,
) -> Result<()> {
    let recipient = if entry.authority == [0u8; 32] {
        cranker.pubkey()
    } else {
        Pubkey::new_from_array(entry.authority)
    };
    let close = ix::close(cranker.pubkey(), entry.pubkey, recipient);
    let blockhash = rpc.get_latest_blockhash().map_err(|e| {
        metrics::metrics()
            .rpc_errors_total
            .with_label_values(&["get_latest_blockhash"])
            .inc();
        anyhow::Error::new(e).context("latest_blockhash")
    })?;
    let mut ixs: Vec<Instruction> = Vec::with_capacity(2);
    if priority_fee_micro_lamports > 0 {
        ixs.push(set_compute_unit_price(priority_fee_micro_lamports));
    }
    ixs.push(close);
    let msg = Message::new_with_blockhash(&ixs, Some(&cranker.pubkey()), &blockhash);
    let tx = Transaction::new(&[cranker], msg, blockhash);
    rpc.send_transaction_with_config(
        &tx,
        RpcSendTransactionConfig {
            skip_preflight: false,
            max_retries: Some(5),
            preflight_commitment: Some(CommitmentLevel::Processed),
            ..Default::default()
        },
    )
    .map_err(|e| {
        metrics::metrics()
            .rpc_errors_total
            .with_label_values(&["send_transaction"])
            .inc();
        anyhow::Error::new(e).context("send_transaction")
    })?;
    Ok(())
}
