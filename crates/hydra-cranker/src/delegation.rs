//! Delegation lifecycle for the cranker's own keypair (ephemeral mode only).
//!
//! On the rollup, a transaction's fee payer may only have its lamports change
//! if the account is `delegated()` — see `access_permissions.rs` in
//! magicblock-svm. The ephemeral `Trigger` credits the cranker its
//! `CRANKER_REWARD`, which *is* a fee-payer balance change, so an undelegated
//! cranker has every trigger rejected with `InvalidAccountForFee` and parks all
//! of its cranks after `MAX_CONSECUTIVE_FAILURES`. Delegation is a hard
//! precondition for ephemeral mode, not an optimization.
//!
//! The cranker keypair is an ordinary on-curve wallet, so this uses the
//! *on-curve* delegation flow, and the cranker funds and signs it all itself —
//! there is no second keypair. See [`ensure_delegated`] for the one non-obvious
//! part of paying for your own delegation.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentLevel;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_rpc_client_api::request::RpcRequest;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_status_client_types::UiTransactionEncoding;

use ephemeral_rollups_sdk::consts::{MAGIC_CONTEXT_ID, MAGIC_PROGRAM_ID};
use ephemeral_rollups_sdk::dlp_api::args::DelegateArgs;
use ephemeral_rollups_sdk::dlp_api::instruction_builder::delegate;
use ephemeral_rollups_sdk::pda::{
    delegation_metadata_pda_from_delegated_account, delegation_record_pda_from_delegated_account,
};

use crate::metrics;

/// Gap between `getDelegationStatus` polls while waiting for the rollup to
/// catch up with a delegation.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Bytes of rent to pre-fund each delegation PDA with. Must be at least their
/// real sizes — `DelegationRecord` is 96 bytes and an on-curve (empty-seed)
/// `DelegationMetadata` is 53 — with margin, since over-funding is free
/// (surplus is refunded when the delegation is torn down) but under-funding
/// re-introduces the payer debit this whole scheme exists to avoid.
const PREFUND_SPACE: usize = 256;

/// `MagicBlockInstruction::ScheduleCommitAndUndelegate`, bincode-encoded: the
/// enum is serialized as a `u32` LE variant index, and this is the third
/// variant (`ModifyAccounts`, `ScheduleCommit`, then this one).
const SCHEDULE_COMMIT_AND_UNDELEGATE: [u8; 4] = 2u32.to_le_bytes();

fn to_local(pk: ephemeral_rollups_sdk::compat::Pubkey) -> Pubkey {
    Pubkey::new_from_array(pk.to_bytes())
}

fn to_sdk(pk: &Pubkey) -> ephemeral_rollups_sdk::compat::Pubkey {
    ephemeral_rollups_sdk::compat::Pubkey::new_from_array(pk.to_bytes())
}

fn delegation_program_id() -> Pubkey {
    to_local(ephemeral_rollups_sdk::consts::DELEGATION_PROGRAM_ID)
}

/// Whether `account` is delegated, judged on the *base* layer: a delegated
/// on-curve wallet is owned by the delegation program there.
fn is_delegated_on_base(base: &RpcClient, account: &Pubkey) -> Result<bool> {
    match base.get_account_with_commitment(account, base.commitment()) {
        Ok(resp) => Ok(resp
            .value
            .is_some_and(|a| a.owner == delegation_program_id())),
        Err(e) => Err(anyhow::Error::new(e).context("read cranker account on base")),
    }
}

/// Ask the *rollup* whether `account` is delegated.
///
/// `getDelegationStatus` is a MagicBlock RPC extension returning
/// `{"isDelegated": bool}`, read straight from the flag the fee-payer check
/// consults, and it resolves the account against the base layer when the rollup
/// has not cloned it yet.
pub fn is_delegated(rpc: &RpcClient, account: &Pubkey) -> Result<bool> {
    let status: serde_json::Value = rpc
        .send(
            RpcRequest::Custom {
                method: "getDelegationStatus",
            },
            serde_json::json!([account.to_string()]),
        )
        .map_err(|e| anyhow::Error::new(e).context("getDelegationStatus"))?;

    status
        .get("isDelegated")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| anyhow!("getDelegationStatus returned no `isDelegated` field: {status}"))
}

/// Delegate the cranker keypair to the rollup's validator, unless it already is,
/// then wait for the rollup to agree.
///
/// Idempotent: a cranker restarting against a rollup it is already delegated to
/// sends no transaction. Returns whether *this* call delegated the account,
/// which is what tells the shutdown path the delegation is ours to release —
/// see [`undelegate`].
///
/// **Paying for your own delegation.** The cranker is the sole signer and payer,
/// which the delegation program does not support directly: `Delegate` requires
/// the account to already be `assign`ed to the delegation program, and once it
/// is, the *system* program can no longer debit it to fund the delegation PDAs —
/// that CPI fails `ExternalAccountLamportSpend`. The way out is that DLP's
/// `create_pda` only transfers from the payer when the target PDA is short of
/// rent. So this pre-funds the record and metadata PDAs *before* the `assign`,
/// while the cranker is still system-owned and can pay. `Delegate` then finds
/// them funded, skips the transfer, and only allocates and assigns them.
pub fn ensure_delegated(base: &RpcClient, er: &RpcClient, cranker: &Keypair) -> Result<bool> {
    let acct = cranker.pubkey();
    if is_delegated_on_base(base, &acct)? {
        log::info!("cranker {acct} is already delegated");
        return Ok(false);
    }

    // Pin the delegation to the rollup we actually crank for; a delegation to
    // some other validator would not let this rollup adopt the account.
    let validator = er
        .get_identity()
        .map_err(|e| anyhow::Error::new(e).context("read rollup validator identity"))?;
    log::info!("delegating cranker {acct} to validator {validator}");

    let record = to_local(delegation_record_pda_from_delegated_account(&to_sdk(&acct)));
    let metadata = to_local(delegation_metadata_pda_from_delegated_account(&to_sdk(
        &acct,
    )));
    let rent = base
        .get_minimum_balance_for_rent_exemption(PREFUND_SPACE)
        .map_err(|e| anyhow::Error::new(e).context("rent exemption for delegation PDAs"))?;

    // Order is load-bearing: both transfers must execute while the cranker is
    // still owned by the system program.
    let ixs = vec![
        solana_system_interface::instruction::transfer(&acct, &record, rent),
        solana_system_interface::instruction::transfer(&acct, &metadata, rent),
        solana_system_interface::instruction::assign(&acct, &delegation_program_id()),
        // Empty `seeds` marks the account on-curve, skipping the PDA
        // seed-derivation check.
        to_local_instruction(delegate(
            to_sdk(&acct),
            to_sdk(&acct),
            None,
            DelegateArgs {
                commit_frequency_ms: u32::MAX,
                seeds: vec![],
                validator: Some(to_sdk(&validator)),
            },
        )),
    ];

    send(base, &ixs, cranker, "delegate cranker")?;
    log::info!("cranker {acct} delegated");
    Ok(true)
}

/// Block until the rollup reports `account` as delegated, retrying until
/// `timeout` elapses.
///
/// Separate from [`ensure_delegated`] because the base-layer transaction landing
/// is not the same as the rollup having cloned the account and seen the flag;
/// the trigger loop needs the latter.
pub fn wait_until_delegated(rpc: &RpcClient, account: &Pubkey, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        // A transient RPC failure should not end the wait, but if the wait ends
        // *on* one, that error explains the timeout better than "not delegated".
        let outcome = is_delegated(rpc, account);
        match &outcome {
            Ok(true) => {
                log::info!("cranker {account} is delegated");
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => log::debug!("delegation check for {account} failed: {e:#}"),
        }
        if Instant::now() >= deadline {
            if let Err(e) = outcome {
                return Err(e.context(format!(
                    "could not determine whether cranker {account} is delegated within {timeout:?}"
                )));
            }
            bail!(
                "cranker {account} is not delegated to this rollup after {timeout:?}. \
                 Ephemeral triggers credit the cranker its reward, and the rollup rejects a \
                 fee payer whose lamports change unless it is delegated, so every trigger \
                 would fail with `InvalidAccountForFee`."
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Commit the cranker's rollup balance back to L1 and release the delegation.
///
/// Submitted on the *rollup*: the delegation program's own client-side
/// undelegate paths do not apply to an on-curve wallet — `RequestUndelegation`
/// rejects on-curve accounts (DLP error 49) and `Undelegate` must be signed by
/// the validator. The magic program accepts it because the account being
/// released signs for itself (`signers.contains(committee_pubkey)`), which is
/// what stands in for the usual owner-program CPI.
///
/// **The instruction's payer must not be the cranker**, which is why `payer` is
/// a throwaway keypair rather than the cranker itself. `process_schedule_commit`
/// marks every committee account undelegated (`set_delegated(false)`) and only
/// *then* calls `charge_delegated_payer`, which requires the payer to still be
/// delegated. With the cranker as both payer and sole committee it clears its
/// own flag and fails its own check with `IllegalOwner`.
///
/// An undelegated payer sidesteps that entirely: `try_get_fee_vault` only
/// demands a magic-fee-vault — and only charges a commit fee — when the payer is
/// itself delegated, so a fresh keypair takes the no-vault path and is never
/// debited. It therefore needs no lamports and no on-chain existence; it only
/// has to sign. The vault must *not* be passed in that case either, or it would
/// be read as the first committee account.
pub fn undelegate(er: &RpcClient, cranker: &Keypair, payer: &Keypair) -> Result<()> {
    let acct = cranker.pubkey();
    log::info!("undelegating cranker {acct} (payer {})", payer.pubkey());
    // `[payer(w,s), magic_context(w), accounts to commit+undelegate..]`. The
    // cranker signs for itself, which is what clears the permission check in
    // place of the usual owner-program CPI, and must be writable or
    // undelegation is refused.
    let ix = Instruction {
        program_id: to_local(MAGIC_PROGRAM_ID),
        accounts: vec![
            // Read-only, unlike the SDK's builder: that marks the payer writable
            // because it expects to charge it, but the rollup rejects a writable
            // account that is not delegated (`InvalidWritableAccount`), and this
            // one is deliberately neither.
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new(to_local(MAGIC_CONTEXT_ID), false),
            AccountMeta::new(acct, true),
        ],
        data: SCHEDULE_COMMIT_AND_UNDELEGATE.to_vec(),
    };
    // The *transaction* fee payer stays the cranker: it is funded and, being
    // delegated, is allowed to have its balance move on the rollup. The
    // throwaway only signs.
    send_with(
        er,
        &[ix],
        &cranker.pubkey(),
        &[cranker, payer],
        "undelegate cranker",
    )?;
    log::info!("cranker {acct} undelegation scheduled");
    Ok(())
}

/// Sign, send, and confirm a one-off lifecycle transaction paid for by `signer`.
fn send(rpc: &RpcClient, ixs: &[Instruction], signer: &Keypair, what: &str) -> Result<()> {
    send_with(rpc, ixs, &signer.pubkey(), &[signer], what)
}

/// [`send`] with an explicit fee payer and signer set.
///
/// `skip_preflight` because on the rollup accounts are cloned from the base only
/// on the real send path, so a simulation would fail on accounts that do not
/// exist there yet. The signature is polled either way, so a revert still
/// surfaces as an error.
fn send_with(
    rpc: &RpcClient,
    ixs: &[Instruction],
    fee_payer: &Pubkey,
    signers: &[&Keypair],
    what: &str,
) -> Result<()> {
    let blockhash = rpc.get_latest_blockhash().map_err(|e| {
        metrics::metrics()
            .rpc_errors_total
            .with_label_values(&["get_latest_blockhash"])
            .inc();
        anyhow::Error::new(e).context("latest_blockhash")
    })?;
    let msg = Message::new_with_blockhash(ixs, Some(fee_payer), &blockhash);
    let tx = Transaction::new(signers, msg, blockhash);
    let sig = rpc
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
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
            anyhow::Error::new(e).context(what.to_string())
        })?;

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rpc.get_signature_status(&sig) {
            Ok(Some(Ok(()))) => return Ok(()),
            // A revert carries only the error code; the program's own `ic_msg!`
            // explanation lives in the transaction logs, so replay it.
            Ok(Some(Err(e))) => {
                bail!("{what}: tx {sig} reverted: {e:?}{}", fetch_logs(rpc, &sig))
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(250)),
            Ok(None) => bail!("{what}: tx {sig} not confirmed within 30s"),
            Err(e) => return Err(anyhow::Error::new(e).context(format!("{what}: confirm"))),
        }
    }
}

/// Fetch a reverted transaction's own program logs, formatted for appending to
/// an error. Best-effort: empty when the node cannot produce them.
///
/// Deliberately *not* a re-simulation. Replaying needs
/// `replace_recent_blockhash` (the original blockhash has usually expired),
/// which invalidates the signature and so forces `sig_verify: false` — and that
/// empties the runtime's signer set. Programs that branch on who signed then
/// fail somewhere else entirely, producing confident, wrong logs. The recorded
/// transaction is the only faithful account of what happened.
fn fetch_logs(rpc: &RpcClient, sig: &Signature) -> String {
    let Ok(tx) = rpc.get_transaction(sig, UiTransactionEncoding::Base64) else {
        return String::new();
    };
    let logs: Option<Vec<String>> = tx
        .transaction
        .meta
        .map(|m| m.log_messages)
        .and_then(Into::into);
    match logs {
        Some(logs) if !logs.is_empty() => format!("\n  logs:\n    {}", logs.join("\n    ")),
        _ => String::new(),
    }
}

/// Rebuild an SDK-built instruction with our own pubkey type, field by field:
/// the SDK is built against an older `solana-pubkey` major than the cranker.
fn to_local_instruction(ix: ephemeral_rollups_sdk::compat::Instruction) -> Instruction {
    Instruction {
        program_id: to_local(ix.program_id),
        accounts: ix
            .accounts
            .into_iter()
            .map(|m| AccountMeta {
                pubkey: to_local(m.pubkey),
                is_signer: m.is_signer,
                is_writable: m.is_writable,
            })
            .collect(),
        data: ix.data,
    }
}
