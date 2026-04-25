//! Hydra integration tests — mollusk-svm based.
//!
//! The on-chain program is loaded from `target/deploy/hydra.so` (relative to
//! the workspace root). Run `cargo build-sbf` first. `cargo test-sbf` does
//! both in one step.
//!
//! The `#[test]` functions below run under `cargo test`. The bench file at
//! `benches/compute_units.rs` also imports setup helpers from this module
//! (hence no `#![cfg(test)]`).

use std::{cell::RefCell, rc::Rc};

use mollusk_svm::{program::keyed_account_for_system_program, Mollusk};
use mollusk_svm_programs_memo::memo;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};
use solana_svm_log_collector::LogCollector;

use hydra_api::{
    consts::{ix, CRANK_HEADER_SIZE, META_FLAG_WRITABLE},
    state::Crank,
};
#[cfg(test)]
use hydra_api::{
    consts::{CRANKER_REWARD, STALENESS_THRESHOLD_SLOTS},
    instruction::{CreateArgs, SchedMeta},
    state::region_len_for,
};

/// Absolute path to the built `.so` (without extension) — mollusk appends `.so`.
pub const HYDRA_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/hydra");

// ---------------------------------------------------------------------------
// Helpers (pub so the bench file `benches/compute_units.rs` can reuse them)
// ---------------------------------------------------------------------------

pub const INSTRUCTIONS_SYSVAR_ID: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");

/// Minimal pinocchio no-op program shipped under `tests/programs/noop/`.
/// Used as a scheduled-ix target so its cost is ~4 CU.
pub const NOOP_ID: Pubkey = pubkey!("4sdZFwGE7TkQCJVpfggvfy2ZwGNCfF6hAMJYjZU5HpZG");

/// Path to the built noop `.so` (without extension).
pub const NOOP_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/hydra_noop");

pub fn hydra_id() -> Pubkey {
    Pubkey::new_from_array(hydra_api::ID.to_bytes())
}

pub fn mollusk_with_hydra() -> Mollusk {
    let id = hydra_id();
    let mut mollusk = Mollusk::new(&id, HYDRA_SO);
    memo::add_program(&mut mollusk);
    mollusk
}

/// Load the tiny pinocchio noop alongside hydra. Call this before using
/// `NOOP_ID` as a scheduled program target.
pub fn load_noop(mollusk: &mut Mollusk) {
    mollusk.add_program(&NOOP_ID, NOOP_SO);
}

pub fn find_crank(seed: &[u8; 32]) -> (Pubkey, u8) {
    let (addr, bump) = hydra_api::state::find_crank_pda(seed);
    (Pubkey::new_from_array(addr.to_bytes()), bump)
}

/// Build a `Create` instruction.
pub fn create_ix(
    payer: Pubkey,
    crank: Pubkey,
    seed: [u8; 32],
    authority: [u8; 32],
    start_slot: u64,
    interval_slots: u64,
    remaining: u64,
    priority_tip: u64,
    cu_limit: u32,
    sched_program: Pubkey,
    sched_metas: &[(Pubkey, bool)], // (pubkey, is_writable)
    sched_data: &[u8],
) -> Instruction {
    let (system_program, _) = keyed_account_for_system_program();

    let mut data = vec![ix::CREATE];
    data.extend_from_slice(&seed);
    data.extend_from_slice(&authority);
    data.extend_from_slice(&start_slot.to_le_bytes());
    data.extend_from_slice(&interval_slots.to_le_bytes());
    data.extend_from_slice(&remaining.to_le_bytes());
    data.extend_from_slice(&priority_tip.to_le_bytes());
    data.extend_from_slice(&cu_limit.to_le_bytes());
    data.push(sched_metas.len() as u8);
    data.extend_from_slice(&(sched_data.len() as u16).to_le_bytes());
    data.extend_from_slice(&sched_program.to_bytes());
    for (pk, w) in sched_metas {
        let flag: u8 = if *w { META_FLAG_WRITABLE } else { 0 };
        data.push(flag);
        data.extend_from_slice(&pk.to_bytes());
    }
    data.extend_from_slice(sched_data);

    Instruction {
        program_id: hydra_id(),
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(crank, false),
            AccountMeta::new_readonly(system_program, false),
        ],
        data,
    }
}

pub fn trigger_ix(crank: Pubkey, cranker: Pubkey) -> Instruction {
    Instruction {
        program_id: hydra_id(),
        accounts: vec![
            AccountMeta::new(crank, false),
            AccountMeta::new(cranker, true),
            AccountMeta::new_readonly(INSTRUCTIONS_SYSVAR_ID, false),
        ],
        data: vec![ix::TRIGGER],
    }
}

pub fn cancel_ix(authority: Pubkey, crank: Pubkey, recipient: Pubkey) -> Instruction {
    Instruction {
        program_id: hydra_id(),
        accounts: vec![
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(crank, false),
            AccountMeta::new(recipient, false),
        ],
        data: vec![ix::CANCEL],
    }
}

pub fn close_ix(reporter: Pubkey, crank: Pubkey, recipient: Pubkey) -> Instruction {
    Instruction {
        program_id: hydra_id(),
        accounts: vec![
            AccountMeta::new(reporter, true),
            AccountMeta::new(crank, false),
            AccountMeta::new(recipient, false),
        ],
        data: vec![ix::CLOSE],
    }
}

pub const SEED: [u8; 32] = [0x11; 32];
pub const PAYER_LAMPORTS: u64 = 1_000_000_000; // 1 SOL

/// Decode the on-chain Crank header from raw account bytes.
pub fn decode_header(data: &[u8]) -> &Crank {
    assert!(data.len() >= CRANK_HEADER_SIZE);
    // SAFETY: Crank is align-1 and we've length-checked.
    unsafe { &*(data.as_ptr() as *const Crank) }
}

/// Pull the last `Program {hydra_id} consumed N ... compute units` line
/// out of a collector's recorded messages and return `N`. The runtime
/// emits one such line per top-level Hydra invocation. Clears collected
/// messages so the next scenario starts fresh.
pub fn take_hydra_cu(logger: &Rc<RefCell<LogCollector>>) -> Option<u64> {
    let mut collector = logger.borrow_mut();
    let needle = format!("Program {} consumed ", hydra_id());
    let cu = collector
        .messages
        .iter()
        .rev()
        .find_map(|m| m.strip_prefix(&needle))
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(n, _)| n.parse::<u64>().ok());
    collector.messages.clear();
    cu
}

// ---------------------------------------------------------------------------
// CU table — shared by `cargo bench -p hydra-tests` and the `cu_table`
// `#[ignore]`d test below.
// ---------------------------------------------------------------------------

/// Run every Hydra scenario through mollusk with a `LogCollector` attached,
/// then print a per-program / per-tx CU table.
pub fn print_cu_table() {
    let mut mollusk = mollusk_with_hydra();
    load_noop(&mut mollusk);
    let logger = LogCollector::new_ref();
    mollusk.logger = Some(logger.clone());

    let payer = Pubkey::new_unique();
    let cranker = Pubkey::new_unique();
    let authority = Pubkey::new_unique();
    let (crank, _bump) = find_crank(&SEED);
    let tick: &[u8] = b"tick";
    let (system_program, sys_acct) = keyed_account_for_system_program();

    // Create
    let create = create_ix(
        payer,
        crank,
        SEED,
        authority.to_bytes(),
        0,
        400,
        10,
        1_000,
        0, // cu_limit
        NOOP_ID,
        &[],
        tick,
    );
    let initial = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank, Account::default()),
        (cranker, Account::new(0, 0, &system_program)),
        (authority, Account::new(0, 0, &system_program)),
        (system_program, sys_acct.clone()),
    ];
    let r_create = mollusk.process_transaction_instructions(&[create], &initial);
    assert!(
        r_create.raw_result.is_ok(),
        "create: {:?}",
        r_create.raw_result
    );
    let cu_create_tx = r_create.compute_units_consumed;
    let cu_create = take_hydra_cu(&logger).expect("hydra log: create");

    let mut funded = r_create.resulting_accounts.clone();
    for (k, a) in funded.iter_mut() {
        if *k == crank {
            a.lamports += 1_000_000;
        }
    }

    // Trigger (happy, 2-ix tx)
    let trigger = trigger_ix(crank, cranker);
    let scheduled = Instruction {
        program_id: NOOP_ID,
        accounts: vec![],
        data: tick.to_vec(),
    };
    let r_trig_ok =
        mollusk.process_transaction_instructions(&[trigger.clone(), scheduled], &funded);
    assert!(
        r_trig_ok.raw_result.is_ok(),
        "trigger happy: {:?}",
        r_trig_ok.raw_result
    );
    assert_eq!(
        decode_header(
            &r_trig_ok
                .resulting_accounts
                .iter()
                .find(|(k, _)| k == &crank)
                .unwrap()
                .1
                .data
        )
        .executed(),
        1
    );
    let cu_trig_tx = r_trig_ok.compute_units_consumed;
    let cu_trig_hydra = take_hydra_cu(&logger).expect("hydra log: trigger happy");

    // Trigger (reject: no follow-up)
    let r_trig_fail = mollusk.process_transaction_instructions(&[trigger], &funded);
    assert!(r_trig_fail.raw_result.is_err());
    let cu_trig_fail_tx = r_trig_fail.compute_units_consumed;
    let cu_trig_fail = take_hydra_cu(&logger).expect("hydra log: trigger reject");

    // Cancel
    let cancel = cancel_ix(authority, crank, authority);
    let r_cancel = mollusk.process_transaction_instructions(&[cancel], &funded);
    assert!(
        r_cancel.raw_result.is_ok(),
        "cancel: {:?}",
        r_cancel.raw_result
    );
    let cu_cancel_tx = r_cancel.compute_units_consumed;
    let cu_cancel = take_hydra_cu(&logger).expect("hydra log: cancel");

    // Close (reject: healthy)
    let close_reject = close_ix(cranker, crank, authority);
    let r_close_rej = mollusk.process_transaction_instructions(&[close_reject], &funded);
    assert!(r_close_rej.raw_result.is_err());
    let cu_close_rej_tx = r_close_rej.compute_units_consumed;
    let cu_close_rej = take_hydra_cu(&logger).expect("hydra log: close reject");

    // Close (happy: underfunded)
    const SEED2: [u8; 32] = [0x22; 32];
    let (crank2, _) = find_crank(&SEED2);
    let payer2 = Pubkey::new_unique();
    let reporter = Pubkey::new_unique();
    let create2 = create_ix(
        payer2,
        crank2,
        SEED2,
        [0u8; 32],
        0,
        400,
        10,
        0,
        0,
        NOOP_ID,
        &[],
        tick,
    );
    let initial2 = vec![
        (payer2, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank2, Account::default()),
        (reporter, Account::new(0, 0, &system_program)),
        (system_program, sys_acct),
    ];
    let r_create2 = mollusk.process_transaction_instructions(&[create2], &initial2);
    assert!(r_create2.raw_result.is_ok());
    let _ = take_hydra_cu(&logger);

    let close_ok = close_ix(reporter, crank2, reporter);
    let r_close_ok =
        mollusk.process_transaction_instructions(&[close_ok], &r_create2.resulting_accounts);
    assert!(
        r_close_ok.raw_result.is_ok(),
        "close ok: {:?}",
        r_close_ok.raw_result
    );
    let cu_close_ok_tx = r_close_ok.compute_units_consumed;
    let cu_close_ok = take_hydra_cu(&logger).expect("hydra log: close ok");

    println!();
    println!("  ┌──────────────────────────────────────┬────────────┬──────────┐");
    println!("  │ Scenario                             │  Hydra CU  │  tx CU   │");
    println!("  ├──────────────────────────────────────┼────────────┼──────────┤");
    println!(
        "  │ Create                               │ {:>10} │ {:>8} │",
        cu_create, cu_create_tx
    );
    println!(
        "  │ Trigger (happy, noop sibling)        │ {:>10} │ {:>8} │",
        cu_trig_hydra, cu_trig_tx
    );
    println!(
        "  │ Trigger (reject: no follow-up)       │ {:>10} │ {:>8} │",
        cu_trig_fail, cu_trig_fail_tx
    );
    println!(
        "  │ Cancel                               │ {:>10} │ {:>8} │",
        cu_cancel, cu_cancel_tx
    );
    println!(
        "  │ Close (reject: healthy)              │ {:>10} │ {:>8} │",
        cu_close_rej, cu_close_rej_tx
    );
    println!(
        "  │ Close (happy: underfunded)           │ {:>10} │ {:>8} │",
        cu_close_ok, cu_close_ok_tx
    );
    println!("  └──────────────────────────────────────┴────────────┴──────────┘");
    println!();
    println!("  >> Hydra-only Trigger (happy): {} CU <<", cu_trig_hydra);
    println!();
    println!(
        "  `Hydra CU` = per-program CU from Solana's `stable_log` — what\n  \
         runs inside Hydra's invocation frame.  `tx CU` = \n  \
         `TransactionResult::compute_units_consumed` (sum of all programs).\n"
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Human-readable per-instruction CU table. Run with:
/// `cargo test -p hydra-tests cu_table -- --ignored --nocapture`
/// (equivalent output to `cargo bench -p hydra-tests`).
#[test]
#[ignore]
fn cu_table() {
    print_cu_table();
}

#[test]
fn public_create_builder_serializes_cu_limit_and_executes() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let scheduled_meta = Pubkey::new_unique();
    let scheduled_data: &[u8] = b"tick";
    let cu_limit: u32 = 321_000;

    let ix = hydra_api::instruction::create(
        payer,
        crank_pda,
        &CreateArgs {
            seed: SEED,
            authority: [0u8; 32],
            start_slot: 7,
            interval_slots: 100,
            remaining: 10,
            priority_tip: 1_000,
            cu_limit,
            scheduled_program_id: memo::ID,
            scheduled_metas: &[SchedMeta::writable(scheduled_meta)],
            scheduled_data,
        },
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (system_program, system_program_acct),
    ];

    let result = mollusk.process_transaction_instructions(&[ix], &accounts);
    assert!(
        result.raw_result.is_ok(),
        "create via public builder failed: {:?}",
        result.raw_result
    );

    let crank_acct = result
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &crank_pda)
        .map(|(_, a)| a)
        .expect("crank account");

    let header = decode_header(&crank_acct.data);
    assert_eq!(header.next_exec_slot(), 7);
    assert_eq!(header.interval_slots(), 100);
    assert_eq!(header.priority_tip(), 1_000);
    assert_eq!(header.cu_limit(), cu_limit);
    assert_eq!(
        header.region_len() as usize,
        region_len_for(1, scheduled_data.len())
    );
}

#[test]
fn create_happy_path_writes_header_and_region() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let recipient = Pubkey::new_unique();
    let memo_data: &[u8] = b"tick";

    let ix = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32], // no cancel authority
        0,         // start_slot: immediately executable
        100,       // interval_slots
        10,        // remaining (not infinite)
        1_000,     // priority_tip
        0,         // cu_limit (0 = omit the ix)
        memo::ID,
        &[(recipient, false)], // one read-only account just for content
        memo_data,
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (system_program, system_program_acct),
    ];

    let result = mollusk.process_transaction_instructions(&[ix], &accounts);
    assert!(
        result.raw_result.is_ok(),
        "create failed: {:?}",
        result.raw_result
    );

    // Find the crank account in the result.
    let crank_acct = result
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &crank_pda)
        .map(|(_, a)| a)
        .expect("crank account");

    assert_eq!(crank_acct.owner, hydra_id(), "owner mismatch");
    let region_len = region_len_for(1, memo_data.len());
    assert_eq!(
        crank_acct.data.len(),
        CRANK_HEADER_SIZE + region_len,
        "total account size"
    );

    let header = decode_header(&crank_acct.data);
    assert_eq!(header.seed, SEED);
    assert_eq!(header.authority, [0u8; 32]);
    assert_eq!(header.next_exec_slot(), 0);
    assert_eq!(header.interval_slots(), 100);
    assert_eq!(header.remaining(), 10);
    assert_eq!(header.priority_tip(), 1_000);
    assert_eq!(header.executed(), 0);
    assert_eq!(header.region_len() as usize, region_len);
    assert!(header.rent_min() > 0, "rent_min should be cached");
}

#[test]
fn trigger_happy_path_pays_reward_and_advances() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let cranker = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let recipient = Pubkey::new_unique();
    let memo_data: &[u8] = b"tick";
    let priority_tip: u64 = 2_500;

    // 1. Create the crank. SPL memo with no metas = log-only, no signers.
    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        0,
        100,
        10,
        priority_tip,
        0, // cu_limit
        memo::ID,
        &[], // scheduled memo takes zero accounts
        memo_data,
    );

    // 2. Trigger + sibling memo ix (must be ix[k+1] in the tx).
    let trigger = trigger_ix(crank_pda, cranker);
    let scheduled = Instruction {
        program_id: memo::ID,
        accounts: vec![],
        data: memo_data.to_vec(),
    };

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let (memo_id, memo_acct) = memo::keyed_account();

    let cranker_starting: u64 = 0;
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (cranker, Account::new(cranker_starting, 0, &system_program)),
        (recipient, Account::new(1_000_000, 0, &system_program)),
        (memo_id, memo_acct),
        (system_program, system_program_acct),
    ];

    // Run create first (separate tx — can't sign the trigger in the same tx
    // since create needs payer-signed and trigger needs cranker-signed, and
    // the cranker didn't fund the PDA).
    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(
        after_create.raw_result.is_ok(),
        "create failed: {:?}",
        after_create.raw_result
    );

    // Top up the crank PDA so it can afford reward + tip above rent_min.
    // In production a user would do this via a direct system transfer to the
    // crank PDA; here we just mutate the in-memory account.
    let mut funded = after_create.resulting_accounts.clone();
    for (k, a) in funded.iter_mut() {
        if *k == crank_pda {
            a.lamports += 1_000_000; // 0.001 SOL headroom
        }
    }

    // Now run [trigger, scheduled] as a single tx so the instructions sysvar
    // contains both and verify_followup sees the sibling at index 1.
    let after_trigger = mollusk.process_transaction_instructions(&[trigger, scheduled], &funded);
    assert!(
        after_trigger.raw_result.is_ok(),
        "trigger failed: {:?}; cu={}",
        after_trigger.raw_result,
        after_trigger.compute_units_consumed
    );

    let total_cu = after_trigger.compute_units_consumed;
    eprintln!("trigger + memo CU: {}", total_cu);

    let crank_acct = after_trigger
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &crank_pda)
        .map(|(_, a)| a)
        .expect("crank after trigger");
    let cranker_acct = after_trigger
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &cranker)
        .map(|(_, a)| a)
        .expect("cranker after trigger");

    assert_eq!(
        cranker_acct.lamports,
        cranker_starting + CRANKER_REWARD + priority_tip,
        "cranker reward"
    );

    let header = decode_header(&crank_acct.data);
    assert_eq!(header.executed(), 1, "executed++");
    assert_eq!(header.remaining(), 9, "remaining--");
    assert_eq!(header.next_exec_slot(), 100, "slot advanced by interval");
}

#[test]
fn cancel_with_matching_authority_refunds_recipient() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let authority = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let recipient = Pubkey::new_unique();

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        authority.to_bytes(),
        0,
        100,
        10,
        0,
        0, // cu_limit
        memo::ID,
        &[],
        b"tick",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (authority, Account::new(0, 0, &system_program)),
        (recipient, Account::new(0, 0, &system_program)),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    let cancel = cancel_ix(authority, crank_pda, recipient);
    let after_cancel =
        mollusk.process_transaction_instructions(&[cancel], &after_create.resulting_accounts);
    assert!(
        after_cancel.raw_result.is_ok(),
        "cancel failed: {:?}",
        after_cancel.raw_result
    );

    let recipient_acct = after_cancel
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &recipient)
        .map(|(_, a)| a)
        .expect("recipient");
    assert!(
        recipient_acct.lamports > 0,
        "recipient should receive crank rent"
    );
    let crank_acct = after_cancel
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &crank_pda)
        .map(|(_, a)| a)
        .expect("crank post-cancel");
    assert_eq!(crank_acct.lamports, 0, "crank drained");
}

#[test]
fn cancel_rejects_unauthorized_signer() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let authority = Pubkey::new_unique();
    let imposter = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let recipient = Pubkey::new_unique();

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        authority.to_bytes(),
        0,
        100,
        10,
        0,
        0, // cu_limit
        memo::ID,
        &[],
        b"tick",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (authority, Account::new(0, 0, &system_program)),
        (imposter, Account::new(0, 0, &system_program)),
        (recipient, Account::new(0, 0, &system_program)),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    let cancel = cancel_ix(imposter, crank_pda, recipient);
    let after_cancel =
        mollusk.process_transaction_instructions(&[cancel], &after_create.resulting_accounts);
    assert!(
        after_cancel.raw_result.is_err(),
        "imposter should be rejected"
    );
}

#[test]
fn create_records_authority_signer_flag() {
    // Provenance witness: header records whether `authority` was actually
    // signed for at Create (i.e. `payer == authority`). Scheduled programs
    // can require this flag to treat `authority` as a real witness.
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);

    let create_signed = create_ix(
        payer,
        crank_pda,
        SEED,
        payer.to_bytes(),
        0,
        100,
        10,
        0,
        0,
        memo::ID,
        &[],
        b"tick",
    );
    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (system_program, system_program_acct.clone()),
    ];
    let r = mollusk.process_transaction_instructions(&[create_signed], &accounts);
    assert!(r.raw_result.is_ok());
    let header = decode_header(
        &r.resulting_accounts
            .iter()
            .find(|(k, _)| k == &crank_pda)
            .unwrap()
            .1
            .data,
    );
    assert_eq!(header.authority_signer, 1, "payer == authority -> flag = 1");

    let payer2 = Pubkey::new_unique();
    let other = Pubkey::new_unique();
    const SEED2: [u8; 32] = [0x33; 32];
    let (crank_pda2, _bump) = find_crank(&SEED2);
    let create_unsigned = create_ix(
        payer2,
        crank_pda2,
        SEED2,
        other.to_bytes(),
        0,
        100,
        10,
        0,
        0,
        memo::ID,
        &[],
        b"tick",
    );
    let accounts2 = vec![
        (payer2, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda2, Account::default()),
        (system_program, system_program_acct),
    ];
    let r2 = mollusk.process_transaction_instructions(&[create_unsigned], &accounts2);
    assert!(r2.raw_result.is_ok());
    let header2 = decode_header(
        &r2.resulting_accounts
            .iter()
            .find(|(k, _)| k == &crank_pda2)
            .unwrap()
            .1
            .data,
    );
    assert_eq!(
        header2.authority_signer, 0,
        "payer != authority -> flag = 0"
    );
}

#[test]
fn cancel_rejects_unkillable_crank() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let anyone = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let recipient = Pubkey::new_unique();

    // authority == [0; 32] makes the crank unkillable via Cancel.
    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        0,
        100,
        10,
        0,
        0, // cu_limit
        memo::ID,
        &[],
        b"tick",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (anyone, Account::new(0, 0, &system_program)),
        (recipient, Account::new(0, 0, &system_program)),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    let cancel = cancel_ix(anyone, crank_pda, recipient);
    let after_cancel =
        mollusk.process_transaction_instructions(&[cancel], &after_create.resulting_accounts);
    assert!(
        after_cancel.raw_result.is_err(),
        "unkillable crank should refuse Cancel"
    );
}

#[test]
fn close_permissionless_when_underfunded() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let reporter = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32], // no authority -> reporter is free to name themselves recipient
        0,
        100,
        10,
        0,
        0, // cu_limit
        memo::ID,
        &[],
        b"tick",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (reporter, Account::new(0, 0, &system_program)),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    // Crank is freshly created with exactly rent_min lamports -> 0 headroom -> underfunded.
    let close = close_ix(reporter, crank_pda, reporter);
    let after =
        mollusk.process_transaction_instructions(&[close], &after_create.resulting_accounts);
    assert!(
        after.raw_result.is_ok(),
        "close failed: {:?}",
        after.raw_result
    );

    let reporter_acct = after
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &reporter)
        .map(|(_, a)| a)
        .expect("reporter");
    assert!(reporter_acct.lamports > 0, "reporter claims bounty");
}

#[test]
fn close_refuses_healthy_crank() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let reporter = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        0,
        100,
        10,
        0,
        0, // cu_limit
        memo::ID,
        &[],
        b"tick",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (reporter, Account::new(0, 0, &system_program)),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    // Fund the crank generously so it's NOT underfunded.
    let mut funded = after_create.resulting_accounts.clone();
    for (k, a) in funded.iter_mut() {
        if *k == crank_pda {
            a.lamports += 10_000_000;
        }
    }

    let close = close_ix(reporter, crank_pda, reporter);
    let after = mollusk.process_transaction_instructions(&[close], &funded);
    assert!(after.raw_result.is_err(), "healthy crank must refuse Close");
}

#[test]
fn close_permissionless_when_stuck() {
    let mut mollusk = mollusk_with_hydra();
    mollusk.sysvars.clock.slot = STALENESS_THRESHOLD_SLOTS + 1;

    let payer = Pubkey::new_unique();
    let reporter = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        0,
        100,
        10,
        0,
        0, // cu_limit
        memo::ID,
        &[],
        b"tick",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (reporter, Account::new(0, 0, &system_program)),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    let mut funded = after_create.resulting_accounts.clone();
    for (k, a) in funded.iter_mut() {
        if *k == crank_pda {
            a.lamports += 10_000_000;
        }
    }

    let close = close_ix(reporter, crank_pda, reporter);
    let after = mollusk.process_transaction_instructions(&[close], &funded);
    assert!(
        after.raw_result.is_ok(),
        "stuck crank should permit Close: {:?}",
        after.raw_result
    );
}

#[test]
fn create_rejects_signer_flag_in_metas() {
    // Manually build data with META_FLAG_SIGNER set on a meta, bypassing
    // the `create_ix` helper which only accepts (pubkey, is_writable).
    use hydra_api::consts::META_FLAG_SIGNER;

    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let fake = Pubkey::new_unique();

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let mut data = vec![ix::CREATE];
    data.extend_from_slice(&SEED);
    data.extend_from_slice(&[0u8; 32]); // authority
    data.extend_from_slice(&0u64.to_le_bytes()); // start_slot
    data.extend_from_slice(&100u64.to_le_bytes()); // interval_slots
    data.extend_from_slice(&10u64.to_le_bytes()); // remaining
    data.extend_from_slice(&0u64.to_le_bytes()); // tip
    data.extend_from_slice(&0u32.to_le_bytes()); // cu_limit
    data.push(1u8); // num_accounts
    data.extend_from_slice(&1u16.to_le_bytes()); // data_len
    data.extend_from_slice(&memo::ID.to_bytes());
    data.push(META_FLAG_SIGNER); // <-- the poisoned flag
    data.extend_from_slice(&fake.to_bytes());
    data.push(0x42);

    let bad_ix = Instruction {
        program_id: hydra_id(),
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(crank_pda, false),
            AccountMeta::new_readonly(system_program, false),
        ],
        data,
    };

    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (system_program, system_program_acct),
    ];
    let result = mollusk.process_transaction_instructions(&[bad_ix], &accounts);
    assert!(
        result.raw_result.is_err(),
        "create must reject signer flag in scheduled metas"
    );
}

#[test]
fn trigger_rejects_before_slot() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let cranker = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let memo_data: &[u8] = b"tick";

    // Start at slot u64::MAX - 1 so current_slot (small) never reaches it.
    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        u64::MAX - 1,
        100,
        10,
        0,
        0, /* cu_limit */
        memo::ID,
        &[],
        memo_data,
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let (memo_id, memo_acct) = memo::keyed_account();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (cranker, Account::new(0, 0, &system_program)),
        (memo_id, memo_acct),
        (system_program, system_program_acct),
    ];
    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    let mut funded = after_create.resulting_accounts.clone();
    for (k, a) in funded.iter_mut() {
        if *k == crank_pda {
            a.lamports += 1_000_000;
        }
    }

    let trigger = trigger_ix(crank_pda, cranker);
    let scheduled = Instruction {
        program_id: memo::ID,
        accounts: vec![],
        data: memo_data.to_vec(),
    };
    let after = mollusk.process_transaction_instructions(&[trigger, scheduled], &funded);
    assert!(
        after.raw_result.is_err(),
        "trigger must refuse before next_exec_slot"
    );
}

#[test]
fn trigger_rejects_when_followup_mismatches() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let cranker = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        0,
        100,
        10,
        0,
        0, /* cu_limit */
        memo::ID,
        &[],
        b"expected",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let (memo_id, memo_acct) = memo::keyed_account();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (cranker, Account::new(0, 0, &system_program)),
        (memo_id, memo_acct),
        (system_program, system_program_acct),
    ];
    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    let mut funded = after_create.resulting_accounts.clone();
    for (k, a) in funded.iter_mut() {
        if *k == crank_pda {
            a.lamports += 1_000_000;
        }
    }

    // Scheduled template says data = b"expected"; submit b"different" instead.
    let trigger = trigger_ix(crank_pda, cranker);
    let wrong = Instruction {
        program_id: memo::ID,
        accounts: vec![],
        data: b"different".to_vec(),
    };
    let after = mollusk.process_transaction_instructions(&[trigger, wrong], &funded);
    assert!(
        after.raw_result.is_err(),
        "trigger must reject mismatched followup"
    );
}

#[test]
fn trigger_decrements_until_exhausted() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let cranker = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let memo_data: &[u8] = b"x";

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        0,
        0,
        2,
        0,
        0, /* cu_limit */
        memo::ID,
        &[],
        memo_data,
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let (memo_id, memo_acct) = memo::keyed_account();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (cranker, Account::new(0, 0, &system_program)),
        (memo_id, memo_acct),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    let mut state = after_create.resulting_accounts.clone();
    // Seed enough lamports for 2 triggers + safety buffer.
    for (k, a) in state.iter_mut() {
        if *k == crank_pda {
            a.lamports += 1_000_000;
        }
    }

    let trigger = trigger_ix(crank_pda, cranker);
    let scheduled = Instruction {
        program_id: memo::ID,
        accounts: vec![],
        data: memo_data.to_vec(),
    };

    // First trigger: remaining 2 -> 1.
    let r1 =
        mollusk.process_transaction_instructions(&[trigger.clone(), scheduled.clone()], &state);
    assert!(r1.raw_result.is_ok(), "1st trigger: {:?}", r1.raw_result);
    state = r1.resulting_accounts;

    // Second trigger: remaining 1 -> 0 (now exhausted).
    let r2 =
        mollusk.process_transaction_instructions(&[trigger.clone(), scheduled.clone()], &state);
    assert!(r2.raw_result.is_ok(), "2nd trigger: {:?}", r2.raw_result);
    state = r2.resulting_accounts;

    let header = {
        let crank = state
            .iter()
            .find(|(k, _)| k == &crank_pda)
            .map(|(_, a)| a)
            .unwrap();
        *decode_header(&crank.data)
    };
    assert_eq!(header.executed(), 2);
    assert_eq!(header.remaining(), 0, "should be exhausted");

    // Third trigger must fail: exhausted.
    let r3 = mollusk.process_transaction_instructions(&[trigger, scheduled], &state);
    assert!(
        r3.raw_result.is_err(),
        "3rd trigger must fail on exhausted crank"
    );
}

#[test]
fn trigger_rejects_without_followup() {
    let mollusk = mollusk_with_hydra();
    let payer = Pubkey::new_unique();
    let cranker = Pubkey::new_unique();
    let (crank_pda, _bump) = find_crank(&SEED);
    let recipient = Pubkey::new_unique();

    let create = create_ix(
        payer,
        crank_pda,
        SEED,
        [0u8; 32],
        0,
        100,
        10,
        0,
        0, // cu_limit
        memo::ID,
        &[],
        b"tick",
    );

    let (system_program, system_program_acct) = keyed_account_for_system_program();
    let (memo_id, memo_acct) = memo::keyed_account();
    let accounts = vec![
        (payer, Account::new(PAYER_LAMPORTS, 0, &system_program)),
        (crank_pda, Account::default()),
        (cranker, Account::new(0, 0, &system_program)),
        (recipient, Account::new(0, 0, &system_program)),
        (memo_id, memo_acct),
        (system_program, system_program_acct),
    ];

    let after_create = mollusk.process_transaction_instructions(&[create], &accounts);
    assert!(after_create.raw_result.is_ok());

    // Trigger alone — no follow-up.
    let trigger = trigger_ix(crank_pda, cranker);
    let result =
        mollusk.process_transaction_instructions(&[trigger], &after_create.resulting_accounts);
    assert!(
        result.raw_result.is_err(),
        "trigger should fail without follow-up, got: {:?}",
        result.raw_result
    );
}
