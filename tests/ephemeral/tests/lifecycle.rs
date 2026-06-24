//! End-to-end tests for Hydra's ephemeral-rollup crank, run against the MagicSVM.
//!
//! Prerequisites (the tests load these prebuilt `.so`s from `target/deploy`):
//!
//! ```sh
//! # from the hydra workspace root — the ephemeral instructions are feature-gated
//! cargo build-sbf -- --features ephemeral
//! cargo build-sbf --manifest-path tests/programs/noop/Cargo.toml
//! # then, from this crate:
//! cargo test
//! ```

use ephemeral_rollups_pinocchio::{
    consts::{EPHEMERAL_VAULT_ID, MAGIC_PROGRAM_ID},
    ephemeral_accounts::rent,
};
use hydra_api::{
    consts::{ix, CRANK_HEADER_SIZE},
    instruction::ScheduledIx,
    state::{load_crank, region_len_for},
};
use magicsvm::{MagicSVM, TransactionTarget};
use solana_account::ReadableAccount;
use solana_address::{address, Address};
use solana_instruction::{account_meta::AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;

const INSTRUCTIONS_SYSVAR_ID: Address = address!("Sysvar1nstructions1111111111111111111111111");
const SYSTEM_PROGRAM_ID: Address = address!("11111111111111111111111111111111");
const NOOP_ID: Address = address!("4sdZFwGE7TkQCJVpfggvfy2ZwGNCfF6hAMJYjZU5HpZG");
const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Lamports the sponsor parks in the crank when creating it.
///
/// On a real MagicBlock ephemeral rollup the crank holds **zero** lamports — its
/// rent lives in the shared vault. MagicSVM's litesvm fork, however, purges any
/// 0-lamport account from its accounts DB when a *later* transaction mutates it
/// (`AccountsDb::add_account` removes `lamports == 0` accounts, with no
/// `ephemeral` exemption). So to exercise the crank's mutating instructions
/// (Trigger) under MagicSVM we keep a token balance in it. Hydra's on-chain
/// logic is unchanged and never depends on this balance.
const CRANK_KEEPALIVE: u64 = 2_000_000;

fn find_crank(seed: &[u8; 32]) -> (Address, u8) {
    hydra_api::state::find_crank_pda(seed)
}

fn workspace_target(name: &str) -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../target/deploy");
    p.push(name);
    p
}

/// Total on-chain size of a crank scheduling `scheds`.
fn crank_size(scheds: &[ScheduledIx]) -> u32 {
    let region: usize = scheds
        .iter()
        .map(|s| region_len_for(s.metas.len(), s.data.len()))
        .sum();
    (CRANK_HEADER_SIZE + region) as u32
}

/// Build a single `CreateEphemeral` instruction: it allocates the crank (Magic
/// CPI, synchronous) and writes the header + scheduled-ix tail in one shot.
#[allow(clippy::too_many_arguments)]
fn create_ephemeral_ix(
    sponsor: Address,
    crank: Address,
    seed: &[u8; 32],
    authority: &[u8; 32],
    start_slot: u64,
    interval_slots: u64,
    remaining: u64,
    priority_tip: u64,
    cu_limit: u32,
    scheds: &[ScheduledIx],
) -> Instruction {
    let mut data = vec![ix::CREATE];
    data.extend_from_slice(seed);
    data.extend_from_slice(authority);
    data.extend_from_slice(&start_slot.to_le_bytes());
    data.extend_from_slice(&interval_slots.to_le_bytes());
    data.extend_from_slice(&remaining.to_le_bytes());
    data.extend_from_slice(&priority_tip.to_le_bytes());
    data.extend_from_slice(&cu_limit.to_le_bytes());
    for s in scheds {
        data.push(s.metas.len() as u8);
        data.extend_from_slice(&(s.data.len() as u16).to_le_bytes());
        data.extend_from_slice(s.program_id.as_ref());
        for meta in s.metas {
            let flag: u8 = if meta.is_writable { 0b0000_0010 } else { 0 };
            data.push(flag);
            data.extend_from_slice(meta.pubkey.as_ref());
        }
        data.extend_from_slice(s.data);
    }
    Instruction {
        program_id: hydra_api::ID,
        accounts: vec![
            AccountMeta::new(sponsor, true),
            AccountMeta::new(crank, false),
            AccountMeta::new(EPHEMERAL_VAULT_ID, false),
            AccountMeta::new_readonly(MAGIC_PROGRAM_ID, false),
        ],
        data,
    }
}

fn trigger_ephemeral_ix(crank: Address, cranker: Address) -> Instruction {
    Instruction {
        program_id: hydra_api::ID,
        accounts: vec![
            AccountMeta::new(crank, false),
            AccountMeta::new(cranker, true),
            AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
        ],
        data: vec![ix::TRIGGER],
    }
}

fn cancel_or_close_ix(disc: u8, signer: Address, crank: Address) -> Instruction {
    Instruction {
        program_id: hydra_api::ID,
        accounts: vec![
            AccountMeta::new(signer, true),
            AccountMeta::new(crank, false),
            AccountMeta::new(EPHEMERAL_VAULT_ID, false),
            AccountMeta::new_readonly(MAGIC_PROGRAM_ID, false),
        ],
        data: vec![disc],
    }
}

/// System-program transfer (`from` must sign). Used only to keep the crank
/// above 0 lamports under MagicSVM — see [`CRANK_KEEPALIVE`].
fn system_transfer_ix(from: Address, to: Address, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes()); // System `Transfer` discriminator.
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(from, true), AccountMeta::new(to, false)],
        data,
    }
}

/// Rebuild the scheduled sibling ix the cranker must place right after Trigger.
fn sched_to_ix(s: &ScheduledIx) -> Instruction {
    Instruction {
        program_id: s.program_id,
        accounts: s
            .metas
            .iter()
            .map(|meta| {
                if meta.is_writable {
                    AccountMeta::new(meta.pubkey, false)
                } else {
                    AccountMeta::new_readonly(meta.pubkey, false)
                }
            })
            .collect(),
        data: s.data.to_vec(),
    }
}

/// Boot a MagicSVM with Hydra + noop loaded and a delegated, funded sponsor.
fn setup() -> (MagicSVM, Keypair) {
    let mut svm = MagicSVM::new();
    svm.add_program_from_file(hydra_api::ID, workspace_target("hydra.so"))
        .unwrap();
    svm.add_program_from_file(NOOP_ID, workspace_target("hydra_noop.so"))
        .unwrap();

    let sponsor = Keypair::new();
    svm.airdrop(&sponsor.pubkey(), LAMPORTS_PER_SOL).unwrap();
    // Delegate the sponsor so it is writable on the ephemeral rollup and can pay
    // ephemeral rent to the vault.
    svm.delegate_account(sponsor.pubkey()).unwrap();
    (svm, sponsor)
}

/// A fresh cranker funded on the base layer. It is synced onto the ephemeral
/// ledger as the (writable, index-0) fee payer when it triggers, so it needs a
/// balance but not delegation. Distinct from the sponsor to show that triggering
/// is permissionless.
fn funded_cranker(svm: &mut MagicSVM) -> Keypair {
    let cranker = Keypair::new();
    svm.airdrop(&cranker.pubkey(), LAMPORTS_PER_SOL).unwrap();
    cranker
}

fn send_ephemeral(svm: &mut MagicSVM, ixs: &[Instruction], signers: &[&Keypair], payer: &Address) {
    let bh = svm.latest_blockhash_for(TransactionTarget::Ephemeral);
    let tx = Transaction::new(signers, Message::new(ixs, Some(payer)), bh);
    svm.send_transaction_to(TransactionTarget::Ephemeral, tx)
        .expect("ephemeral tx should succeed");
    svm.expire_blockhash_for(TransactionTarget::Ephemeral);
}

/// Create a crank scheduling one noop, in a single `CreateEphemeral`
/// instruction. A keepalive transfer rides along in the same tx so the crank
/// survives later mutations (MagicSVM purges 0-lamport accounts on mutation).
fn create_crank(
    svm: &mut MagicSVM,
    sponsor: &Keypair,
    seed: [u8; 32],
    authority: [u8; 32],
    interval: u64,
    remaining: u64,
    sched: &ScheduledIx,
) -> Address {
    let (crank, _bump) = find_crank(&seed);
    send_ephemeral(
        svm,
        &[
            create_ephemeral_ix(
                sponsor.pubkey(),
                crank,
                &seed,
                &authority,
                0,
                interval,
                remaining,
                0,
                0,
                std::slice::from_ref(sched),
            ),
            system_transfer_ix(sponsor.pubkey(), crank, CRANK_KEEPALIVE),
        ],
        &[sponsor],
        &sponsor.pubkey(),
    );
    crank
}

fn noop_sched<'a>() -> ScheduledIx<'a> {
    ScheduledIx {
        program_id: NOOP_ID,
        metas: &[],
        data: &[0u8],
    }
}

#[test]
fn create_materializes_hydra_owned_ephemeral_account() {
    let (mut svm, sponsor) = setup();
    let seed = [1u8; 32];
    let (crank, _) = find_crank(&seed);
    let sched = noop_sched();
    let data_len = crank_size(std::slice::from_ref(&sched));

    let vault_before = svm
        .get_shared_account_for(TransactionTarget::Ephemeral, &EPHEMERAL_VAULT_ID)
        .unwrap()
        .lamports();

    // Create with no keepalive: the crank ends the tx at 0 lamports, exercising
    // the pure ephemeral-account materialization.
    send_ephemeral(
        &mut svm,
        &[create_ephemeral_ix(
            sponsor.pubkey(),
            crank,
            &seed,
            &[0u8; 32],
            0,
            1,
            3,
            0,
            0,
            std::slice::from_ref(&sched),
        )],
        &[&sponsor],
        &sponsor.pubkey(),
    );

    let acct = svm
        .get_shared_account_for(TransactionTarget::Ephemeral, &crank)
        .expect("crank should exist on the ephemeral ledger");
    assert!(acct.ephemeral(), "crank must carry the ephemeral flag");
    assert_eq!(acct.owner(), &hydra_api::ID, "crank must be owned by Hydra");
    assert_eq!(acct.data().len(), data_len as usize);
    assert_eq!(acct.lamports(), 0, "ephemeral accounts hold no lamports");

    let vault_after = svm
        .get_shared_account_for(TransactionTarget::Ephemeral, &EPHEMERAL_VAULT_ID)
        .unwrap()
        .lamports();
    assert_eq!(
        vault_after - vault_before,
        rent(data_len),
        "sponsor must pay the per-byte rent into the vault"
    );
}

#[test]
fn create_writes_header_and_tail() {
    let (mut svm, sponsor) = setup();
    let seed = [2u8; 32];
    let authority = sponsor.pubkey().to_bytes();
    let sched = noop_sched();
    let crank = create_crank(&mut svm, &sponsor, seed, authority, 5, 3, &sched);

    let acct = svm
        .get_shared_account_for(TransactionTarget::Ephemeral, &crank)
        .unwrap();
    let data = acct.data();
    let state = unsafe { load_crank(data).unwrap() };
    assert_eq!(state.seed, seed);
    assert_eq!(state.authority, authority);
    assert_eq!(state.next_exec_slot(), 0);
    assert_eq!(state.interval_slots(), 5);
    assert_eq!(state.remaining(), 3);
    assert_eq!(state.executed(), 0);
    assert_eq!(state.authority_signer, 1);
    let region = region_len_for(sched.metas.len(), sched.data.len());
    assert_eq!(state.region_len() as usize, region);
    assert_eq!(data.len(), CRANK_HEADER_SIZE + region);
}

#[test]
fn trigger_runs_scheduled_ix_and_advances() {
    let (mut svm, sponsor) = setup();
    let seed = [3u8; 32];
    let sched = noop_sched();
    let crank = create_crank(&mut svm, &sponsor, seed, [0u8; 32], 1, 3, &sched);

    let cranker = funded_cranker(&mut svm);
    send_ephemeral(
        &mut svm,
        &[
            trigger_ephemeral_ix(crank, cranker.pubkey()),
            sched_to_ix(&sched),
        ],
        &[&cranker],
        &cranker.pubkey(),
    );

    let acct = svm
        .get_shared_account_for(TransactionTarget::Ephemeral, &crank)
        .unwrap();
    let state = unsafe { load_crank(acct.data()).unwrap() };
    assert_eq!(state.executed(), 1);
    assert_eq!(state.remaining(), 2);
    assert_eq!(state.next_exec_slot(), 1);
    // Trigger moves no lamports — the keepalive balance is untouched.
    assert_eq!(acct.lamports(), CRANK_KEEPALIVE);
}

#[test]
fn trigger_rejects_mismatched_followup() {
    let (mut svm, sponsor) = setup();
    let seed = [4u8; 32];
    let sched = noop_sched();
    let crank = create_crank(&mut svm, &sponsor, seed, [0u8; 32], 1, 3, &sched);

    // Sibling noop with different data than the stored template.
    let wrong = ScheduledIx {
        program_id: NOOP_ID,
        metas: &[],
        data: &[9u8],
    };
    let cranker = funded_cranker(&mut svm);
    let bh = svm.latest_blockhash_for(TransactionTarget::Ephemeral);
    let tx = Transaction::new(
        &[&cranker],
        Message::new(
            &[
                trigger_ephemeral_ix(crank, cranker.pubkey()),
                sched_to_ix(&wrong),
            ],
            Some(&cranker.pubkey()),
        ),
        bh,
    );
    assert!(
        svm.send_transaction_to(TransactionTarget::Ephemeral, tx)
            .is_err(),
        "a mismatched follow-up ix must be rejected"
    );
}

#[test]
fn cancel_by_authority_closes_and_refunds() {
    let (mut svm, sponsor) = setup();
    let seed = [5u8; 32];
    let authority = sponsor.pubkey().to_bytes();
    let sched = noop_sched();
    let crank = create_crank(&mut svm, &sponsor, seed, authority, 1, 3, &sched);

    assert!(svm
        .get_shared_account_for(TransactionTarget::Ephemeral, &crank)
        .is_some());

    send_ephemeral(
        &mut svm,
        &[cancel_or_close_ix(ix::CANCEL, sponsor.pubkey(), crank)],
        &[&sponsor],
        &sponsor.pubkey(),
    );

    let after = svm.get_shared_account_for(TransactionTarget::Ephemeral, &crank);
    assert!(
        after
            .map(|a| a.lamports() == 0 && a.data().is_empty())
            .unwrap_or(true),
        "crank should be closed on the ephemeral ledger"
    );
}

#[test]
fn close_permissionless_when_exhausted() {
    let (mut svm, sponsor) = setup();
    let seed = [6u8; 32];
    let sched = noop_sched();
    // authority = none, remaining = 1 → one trigger exhausts it.
    let crank = create_crank(&mut svm, &sponsor, seed, [0u8; 32], 1, 1, &sched);

    let cranker = funded_cranker(&mut svm);
    send_ephemeral(
        &mut svm,
        &[
            trigger_ephemeral_ix(crank, cranker.pubkey()),
            sched_to_ix(&sched),
        ],
        &[&cranker],
        &cranker.pubkey(),
    );

    // A random reporter (not the sponsor) may close an exhausted, authority-less crank.
    let reporter = Keypair::new();
    svm.airdrop(&reporter.pubkey(), LAMPORTS_PER_SOL).unwrap();
    svm.delegate_account(reporter.pubkey()).unwrap();
    send_ephemeral(
        &mut svm,
        &[cancel_or_close_ix(ix::CLOSE, reporter.pubkey(), crank)],
        &[&reporter],
        &reporter.pubkey(),
    );

    let after = svm.get_shared_account_for(TransactionTarget::Ephemeral, &crank);
    assert!(
        after
            .map(|a| a.lamports() == 0 && a.data().is_empty())
            .unwrap_or(true),
        "exhausted crank should be closed"
    );
}
