//! Live end-to-end test for Hydra's ephemeral-rollup crank.
//!
//! Unlike the in-process `tests/ephemeral` suite (which drives MagicSVM), this
//! boots the *real* three-process stack and asserts the cranker keeps a handful
//! of ephemeral cranks firing on schedule. Two scenarios cover both discovery
//! paths: cranks created **before** the cranker starts (its bootstrap
//! `getProgramAccounts` scan) and **after** (live `programSubscribe`
//! notifications, with the bootstrap proven empty first).
//!
//! ```text
//!   mb-test-validator  ── base L1 (delegation + magic programs preloaded),
//!                          hydra(ephemeral) + noop loaded at genesis
//!         ▲  clones programs/accounts on demand
//!         │
//!   ephemeral-validator ── the rollup; hosts the ephemeral crank accounts
//!         ▲  RPC + WS (slotSubscribe / programSubscribe)
//!         │
//!   hydra-cranker       ── watches the rollup, fires `Trigger` every interval
//! ```
//!
//! Execution progress is observed via `logsSubscribe` on the noop program: each
//! crank passes a distinct `u64` id in the scheduled noop's instruction data,
//! and the noop logs `noop-fired:<id>` when it runs. That mirrors how the
//! cranker itself learns about cranks (bootstrap / `programSubscribe`) and
//! fires on slot ticks — the test never polls crank account `executed`.
//!
//! ## Prerequisites
//!
//! Binaries on `PATH` (installed via the `@magicblock-labs/ephemeral-validator`
//! npm package): `mb-test-validator`, `ephemeral-validator`. And the two
//! prebuilt on-chain `.so`s (the rollup clones them from the base):
//!
//! ```sh
//! # from the hydra workspace root
//! cargo build-sbf -- --features ephemeral                        # target/deploy/hydra.so
//! cargo build-sbf --manifest-path tests/programs/noop/Cargo.toml # target/deploy/hydra_noop.so
//! ```
//!
//! The `hydra-cranker` binary is built automatically by the test (see
//! `build_cranker`); it selects the ephemeral program at runtime via
//! `--ephemeral`. Then, from this crate:
//!
//! ```sh
//! cargo test -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The test is `#[ignore]` by default: it spawns external validators, binds
//! local ports, and takes ~10–15s, so it should not run in a plain `cargo test`.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::RecvTimeoutError;
use hydra_api::instruction::{self as ix, ScheduledIx};
use hydra_api::instruction::{
    ephemeral::{self as eph},
    CreateArgs,
};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_pubsub_client::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use solana_signer::Signer;
use solana_system_interface::instruction as system_instruction;
use solana_transaction::Transaction;

// --- Topology ---------------------------------------------------------------

/// Base L1 JSON-RPC port. solana-test-validator serves PubSub on `PORT + 1`.
const BASE_RPC_PORT: u16 = 7101;
const BASE_WS_PORT: u16 = BASE_RPC_PORT + 1;
/// Ephemeral rollup RPC port. Aperture serves RPC + WS on the same address,
/// which is exactly what the cranker's `http→ws` URL derivation assumes.
const ER_RPC_PORT: u16 = 7799;

/// The bundled noop program — its on-chain address is its build keypair's
/// pubkey (`target/deploy/hydra_noop-keypair.json`). Scheduled cranks point at
/// it; it ignores its accounts and data.
const NOOP_ID: &str = "CftjNLnvyBFcEqShc2VRcESCpPnUfDaFub1wgnGCqtHv";

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

// --- Crank parameters -------------------------------------------------------

/// How many cranks to create and watch.
const NUM_CRANKS: usize = 10;
/// Slots between executions. Small so the test stays quick; the rollup runs at
/// ~50 ms/slot, so this is a few seconds per fire.
const INTERVAL_SLOTS: u64 = 1;
/// Executions each crank must reach before the test passes. Proving the crank
/// fires *repeatedly* (not just once) is the point.
const TARGET_EXECUTIONS: u64 = 3;

/// Prefix emitted by [`tests/programs/noop`] when the scheduled ix carries an
/// 8-byte LE crank id in its instruction data.
const NOOP_FIRED_PREFIX: &str = "noop-fired:";

/// Rollup slot time (~50 ms). The e2e crate does not enable `hydra-api/ephemeral`,
/// so `SLOT_FREQUENCY_MS` from that crate reflects base-layer timing.
const ROLLUP_SLOT_MS: u64 = 50;

/// Overall wall-clock budget for all cranks to reach `TARGET_EXECUTIONS`.
/// Derived from rollup slot timing (not base-layer `SLOT_FREQUENCY_MS`).
const EXEC_DEADLINE: Duration =
    Duration::from_millis(ROLLUP_SLOT_MS * INTERVAL_SLOTS * TARGET_EXECUTIONS * 100);

// --- The tests ---------------------------------------------------------------

/// When the cranks are created relative to the cranker starting.
#[derive(Clone, Copy, PartialEq)]
enum CreateOrder {
    /// Cranks exist before the cranker boots — picked up by its bootstrap
    /// `getProgramAccounts` scan.
    BeforeCranker,
    /// Cranks are created after the cranker is already running with an empty
    /// cache — picked up via live `programSubscribe` notifications.
    AfterCranker,
}

// Both tests bind the same fixed local ports and spawn validators, so they must
// not run concurrently. cargo runs tests in a binary on multiple threads, so a
// process-wide lock serializes them (recovering from a poisoned lock if one
// test panics).
static STACK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Cranks created *before* the cranker starts → discovered by its bootstrap scan.
#[test]
#[ignore = "spawns live validators + cranker; run with --ignored"]
fn ephemeral_cranks_fire_on_schedule() {
    run_scenario(CreateOrder::BeforeCranker);
}

/// Cranks created *after* the cranker starts → discovered via `programSubscribe`.
#[test]
#[ignore = "spawns live validators + cranker; run with --ignored"]
fn cranker_catches_cranks_created_after_start() {
    run_scenario(CreateOrder::AfterCranker);
}

fn run_scenario(order: CreateOrder) {
    let _guard = STACK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_ports_free().expect("e2e ports available");
    let tmp = TempDir::new().expect("temp dir");
    let mut stack = Stack {
        children: Vec::new(),
        tmp,
    };
    let result = body(&mut stack, order);
    if let Err(ref e) = result {
        eprintln!("\n[stack] FAILED: {e:#}\n[stack] dumping validator/cranker logs:");
        for f in ["base.log", "er.log", "cranker.log"] {
            dump_log(stack.tmp.path(), f);
        }
    }
    // Tear the stack down (and delete the temp dir) *before* asserting, so a
    // panic never leaks child processes.
    drop(stack);
    result.expect("e2e ephemeral crank test");
}

fn body(stack: &mut Stack, order: CreateOrder) -> Result<()> {
    let tmp = stack.tmp.path().to_path_buf();

    // Resolve build artifacts up front so a missing prerequisite fails fast.
    let hydra_so = artifact("target/deploy/hydra.so")?;
    let noop_so = artifact("target/deploy/hydra_noop.so")?;
    // The cranker MUST be the `ephemeral` build (it targets the `eHyd…` program
    // and skips the lamport-funding checks). A plain `cargo build -p
    // hydra-cranker` overwrites the same path with the base-program build, so we
    // (re)build the ephemeral variant here to guarantee the right binary.
    let cranker_bin = build_cranker()?;
    let hydra_id = ix::EPHEMERAL_PROGRAM_ID.to_string();
    let noop_id = Pubkey::from_str(NOOP_ID).unwrap();

    // `sponsor` is delegated (signs CreateEphemeral, pays rent); `fee_payer` is
    // a plain system wallet that covers tx fees (the delegated sponsor can't be
    // a fee payer); `cranker` pays for the triggers it submits.
    let sponsor = Keypair::new();
    let fee_payer = Keypair::new();
    let cranker = Keypair::new();
    let cranker_kp_path = write_keypair(&tmp, "cranker.json", &cranker)?;

    // 1. Base L1: delegation/magic programs come preloaded by mb-test-validator;
    //    we add hydra(ephemeral) + noop at genesis so the rollup can clone them.
    eprintln!("[stack] starting mb-test-validator on :{BASE_RPC_PORT}");
    let base = spawn(
        "mb-test-validator",
        &[
            "--reset",
            "--quiet",
            "--ledger",
            tmp.join("base-ledger").to_str().unwrap(),
            "--rpc-port",
            &BASE_RPC_PORT.to_string(),
            "--bind-address",
            "127.0.0.1",
            "--bpf-program",
            &hydra_id,
            hydra_so.to_str().unwrap(),
            "--bpf-program",
            NOOP_ID,
            noop_so.to_str().unwrap(),
        ],
        fs::File::create(tmp.join("base.log"))?,
        &[],
    )?;
    stack.push("mb-test-validator", base);

    let base_rpc = RpcClient::new_with_commitment(
        format!("http://127.0.0.1:{BASE_RPC_PORT}"),
        CommitmentConfig::confirmed(),
    );
    wait_for_rpc(&base_rpc, "base", Duration::from_secs(10))?;

    // 2. Fund the three keypairs on the base, then delegate the sponsor so the
    //    rollup will let it spend its own lamports (the rent paid inside
    //    CreateEphemeral). The fee_payer and cranker stay plain system wallets.
    airdrop(&base_rpc, &sponsor.pubkey(), 100 * LAMPORTS_PER_SOL)?;
    airdrop(&base_rpc, &fee_payer.pubkey(), 10 * LAMPORTS_PER_SOL)?;
    airdrop(&base_rpc, &cranker.pubkey(), 10 * LAMPORTS_PER_SOL)?;
    delegate_sponsor(&base_rpc, &sponsor, &fee_payer)?;

    eprintln!(
        "[stack] funded + delegated sponsor {} (fee_payer {}, cranker {})",
        sponsor.pubkey(),
        fee_payer.pubkey(),
        cranker.pubkey()
    );

    // 3. Ephemeral rollup, syncing against the base. It clones the sponsor (as a
    //    delegated account), the hydra + noop programs, and the magic vault on
    //    demand from the base.
    eprintln!("[stack] starting ephemeral-validator on :{ER_RPC_PORT}");
    let er = spawn(
        "ephemeral-validator",
        &[
            "--no-tui",
            "--reset",
            "--lifecycle",
            "ephemeral",
            "--remotes",
            &format!("http://127.0.0.1:{BASE_RPC_PORT}"),
            "--remotes",
            &format!("ws://127.0.0.1:{BASE_WS_PORT}"),
            "--listen",
            &format!("127.0.0.1:{ER_RPC_PORT}"),
            "--storage",
            tmp.join("er-storage").to_str().unwrap(),
        ],
        fs::File::create(tmp.join("er.log"))?,
        &[("RUST_LOG", "warn")],
    )?;
    stack.push("ephemeral-validator", er);

    let er_rpc = RpcClient::new_with_commitment(
        format!("http://127.0.0.1:{ER_RPC_PORT}"),
        CommitmentConfig::confirmed(),
    );
    wait_for_rpc(&er_rpc, "rollup", Duration::from_secs(10))?;
    // Give the rollup's remote-account-provider WS pool a moment to warm up
    // before we make it clone accounts from the base.
    std::thread::sleep(Duration::from_secs(3));

    // Subscribe to noop execution logs before any crank can fire so we never
    // miss an early trigger while the cranker is still bootstrapping.
    let log_watcher = LogFireWatcher::spawn(noop_id)?;

    // 4. Create the cranks and start the cranker, in the order this scenario
    //    exercises. `BeforeCranker` is the bootstrap path; `AfterCranker` is the
    //    live `programSubscribe` path (the cranker boots with an empty cache and
    //    must catch the new cranks from notifications).
    let cranks = match order {
        CreateOrder::BeforeCranker => {
            let cranks = create_cranks(&sponsor, &fee_payer, noop_id)?;
            spawn_cranker(stack, &cranker_bin, &cranker_kp_path, &tmp)?;
            cranks
        }
        CreateOrder::AfterCranker => {
            spawn_cranker(stack, &cranker_bin, &cranker_kp_path, &tmp)?;
            // Let the cranker bootstrap (finding zero cranks) and connect its
            // programSubscribe watcher before any crank exists.
            std::thread::sleep(Duration::from_millis(500));
            assert_cranker_bootstrapped_empty(&tmp)?;
            eprintln!("[stack] cranker is up with an empty cache; creating cranks now");
            create_cranks(&sponsor, &fee_payer, noop_id)?
        }
    };

    // 6. Wait until every crank has fired enough times. Each scheduled noop
    //    carries a distinct id in its ix data; the noop program logs
    //    `noop-fired:<id>` and the watcher attributes fires from those notifications.
    let watch = log_watcher.wait_until(EXEC_DEADLINE)?;

    // 7. Report + assert.
    let mut failures = Vec::new();
    for (i, fires) in watch.fires.iter().enumerate() {
        eprintln!("[result] crank {i} ({}): fires={fires:?}", cranks[i]);

        let executed = fires.iter().filter(|fire| fire.is_some()).count();
        if executed < TARGET_EXECUTIONS as usize {
            failures.push(format!(
                "crank {i} only fired {executed}/{TARGET_EXECUTIONS} times within {EXEC_DEADLINE:?}"
            ));
        }
    }
    if !failures.is_empty() {
        bail!(
            "{} crank(s) failed to keep schedule:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }

    eprintln!("[stack] all {NUM_CRANKS} cranks fired ≥{TARGET_EXECUTIONS}× on schedule");
    Ok(())
}

// --- Scenario helpers -------------------------------------------------------

/// Fail fast when fixed local ports are still held by a prior run's validators.
fn assert_ports_free() -> Result<()> {
    use std::net::TcpListener;
    for (port, label) in [
        (BASE_RPC_PORT, "mb-test-validator"),
        (ER_RPC_PORT, "ephemeral-validator"),
    ] {
        if TcpListener::bind(("127.0.0.1", port)).is_err() {
            bail!(
                "127.0.0.1:{port} is already in use ({label}). \
                 A prior e2e run may have left validators running after Ctrl+C. \
                 Stop them with: pkill -INT -f 'mb-test-validator|ephemeral-validator|hydra-cranker'"
            );
        }
    }
    Ok(())
}

/// Create `NUM_CRANKS` cranks on the rollup in parallel, asserting each materialized.
/// Returns their PDAs in index order.
fn create_cranks(sponsor: &Keypair, fee_payer: &Keypair, noop_id: Pubkey) -> Result<Vec<Pubkey>> {
    let rpc_url = format!("http://127.0.0.1:{ER_RPC_PORT}");
    let commitment = CommitmentConfig::confirmed();
    let mut cranks = vec![Pubkey::default(); NUM_CRANKS];

    std::thread::scope(|scope| {
        let mut handles: Vec<
            std::thread::ScopedJoinHandle<'_, Result<(usize, Pubkey), anyhow::Error>>,
        > = Vec::with_capacity(NUM_CRANKS);
        for i in 0..NUM_CRANKS {
            let rpc_url = rpc_url.clone();
            handles.push(scope.spawn(move || {
                let rpc = RpcClient::new_with_commitment(rpc_url, commitment);
                let crank = create_crank(&rpc, sponsor, fee_payer, crank_seed(i), noop_id, i)?;
                assert_crank_exists(&rpc, &crank).with_context(|| {
                    format!("crank {i} ({crank}) was not created on the rollup")
                })?;
                Ok((i, crank))
            }));
        }
        for handle in handles {
            let (i, crank) = handle
                .join()
                .map_err(|_| anyhow!("create crank thread panicked"))??;
            cranks[i] = crank;
        }
        Ok::<(), anyhow::Error>(())
    })?;

    eprintln!("[stack] created {NUM_CRANKS} crank(s)");
    Ok(cranks)
}

/// A distinct 32-byte crank seed for index `i` (LE-encoded, so it stays unique
/// well past 255 cranks).
fn crank_seed(i: usize) -> [u8; 32] {
    let mut seed = [0u8; 32];
    seed[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
    seed
}

/// Spawn the cranker against the rollup and register it for teardown.
fn spawn_cranker(
    stack: &mut Stack,
    cranker_bin: &Path,
    cranker_kp_path: &Path,
    tmp: &Path,
) -> Result<()> {
    eprintln!("[stack] starting hydra-cranker → rollup");
    let cranker = spawn(
        cranker_bin.to_str().unwrap(),
        &[
            "--rpc-url",
            &format!("http://127.0.0.1:{ER_RPC_PORT}"),
            // The rollup serves RPC and WebSocket on separate ports (RPC+1),
            // unlike the cranker's default same-port `http→ws` derivation.
            "--ws-url",
            &format!("ws://127.0.0.1:{}", ER_RPC_PORT + 1),
            "--keypair",
            cranker_kp_path.to_str().unwrap(),
            // Target the ephemeral-rollup program (runtime selection, not a
            // build feature).
            "--ephemeral",
            // Triggers must skip preflight: the rollup only clones referenced
            // accounts on the real send path, not during preflight simulation.
            "--trigger-skip-preflight",
        ],
        fs::File::create(tmp.join("cranker.log"))?,
        &[("RUST_LOG", "info")],
    )?;
    stack.push("hydra-cranker", cranker);
    Ok(())
}

/// Assert the cranker's startup bootstrap found zero cranks — proving that any
/// cranks it later fires were discovered via live `programSubscribe`
/// notifications, not the bootstrap scan. The cranker logs
/// `bootstrap: N crank(s) cached` at startup (info level).
fn assert_cranker_bootstrapped_empty(tmp: &Path) -> Result<()> {
    let log = tmp.join("cranker.log");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let contents = fs::read_to_string(&log).unwrap_or_default();
        if contents.contains("bootstrap: 0 crank(s) cached") {
            return Ok(());
        }
        // Guard against a cranker that somehow saw cranks at bootstrap.
        if let Some(line) = contents.lines().find(|l| l.contains("crank(s) cached")) {
            bail!("cranker bootstrap was not empty: {line:?}");
        }
        if Instant::now() >= deadline {
            bail!("cranker did not log a bootstrap result within 10s");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// --- Sponsor delegation -----------------------------------------------------

/// Make the sponsor an **on-curve delegated** account so the rollup will let it
/// spend its own lamports (paying the ephemeral-account rent inside
/// `CreateEphemeral`). On the rollup, a fee payer whose lamports change must be
/// `delegated()` (`magicblock-svm/.../access_permissions.rs`) — and an
/// undelegated escrow does *not* set that flag on the wallet. The supported
/// path is the on-curve delegation flow (see MagicBlock's `oncurve-delegation`
/// example): the wallet `assign`s itself to the delegation program, then is
/// delegated to the validator, in one base-layer transaction.
///
/// A separate, system-owned `fee_payer` covers the transaction fee — the
/// delegated sponsor can't be the fee payer (it's owned by the delegation
/// program), and a delegated account also can't `system_program::transfer`.
fn delegate_sponsor(base: &RpcClient, sponsor: &Keypair, fee_payer: &Keypair) -> Result<()> {
    let dlp = delegation_program_id();
    let system = system_program_id();
    let acct = sponsor.pubkey();

    // The wallet reassigns its own owner to the delegation program (it signs).
    let assign = system_instruction::assign(&acct, &dlp);

    // delegation program `Delegate` (disc 0). PDAs: delegate buffer under the
    // *owner* (system) program, record + metadata under the delegation program.
    let (buffer, _) = Pubkey::find_program_address(&[b"buffer", acct.as_ref()], &system);
    let (record, _) = Pubkey::find_program_address(&[b"delegation", acct.as_ref()], &dlp);
    let (metadata, _) =
        Pubkey::find_program_address(&[b"delegation-metadata", acct.as_ref()], &dlp);
    // Data: disc u64 LE, borsh `{ commit_frequency_ms: u32, seeds: Vec<Vec<u8>>,
    // validator: Option<Pubkey> }`. Empty seeds → on-curve account (no PDA
    // seed check); validator pinned to the rollup so it adopts the delegation.
    let mut data = Vec::new();
    data.extend_from_slice(&0u64.to_le_bytes());
    data.extend_from_slice(&u32::MAX.to_le_bytes()); // commit_frequency_ms
    data.extend_from_slice(&0u32.to_le_bytes()); // seeds: empty
    data.push(1u8); // validator: Some(..)
    data.extend_from_slice(er_validator_identity().as_ref());
    let delegate = Instruction {
        program_id: dlp,
        accounts: vec![
            AccountMeta::new(fee_payer.pubkey(), true), // payer
            AccountMeta::new(acct, true),               // delegated account, signs
            AccountMeta::new_readonly(system, false),   // owner program
            AccountMeta::new(buffer, false),            // delegate buffer PDA
            AccountMeta::new(record, false),            // delegation record PDA
            AccountMeta::new(metadata, false),          // delegation metadata PDA
            AccountMeta::new_readonly(system, false),
        ],
        data,
    };
    send(
        base,
        &[assign, delegate],
        &fee_payer.pubkey(),
        &[fee_payer, sponsor],
    )
    .context("delegate sponsor (on-curve)")
}

// --- Crank helpers ----------------------------------------------------------

/// Build + send one `CreateEphemeral` for the crank derived from `seed`,
/// scheduling a single noop. `sponsor` (delegated) signs and pays the rent;
/// `fee_payer` (system-owned) covers the transaction fee. Returns the crank PDA.
fn create_crank(
    rpc: &RpcClient,
    sponsor: &Keypair,
    fee_payer: &Keypair,
    seed: [u8; 32],
    noop: Pubkey,
    crank_index: usize,
) -> Result<Pubkey> {
    let (crank, _bump) = eph::find_crank_pda(&seed);
    let fire_id = crank_fire_id(crank_index);
    let sched = ScheduledIx {
        program_id: noop,
        metas: &[],
        data: &fire_id.to_le_bytes(),
    };
    let create = eph::create(
        sponsor.pubkey(),
        crank,
        &CreateArgs {
            seed,
            authority: [0u8; 32], // no cancel authority → permissionless, runs forever
            start_slot: 0,
            interval_slots: INTERVAL_SLOTS,
            remaining: TARGET_EXECUTIONS,
            priority_tip: 0,
            cu_limit: 0,
            scheduled: std::slice::from_ref(&sched),
        },
    );
    let fund_ix = system_instruction::transfer(&sponsor.pubkey(), &crank, LAMPORTS_PER_SOL / 100);
    send(
        rpc,
        &[create, fund_ix],
        &fee_payer.pubkey(),
        &[fee_payer, sponsor],
    )
    .context("create crank on rollup")?;
    Ok(crank)
}

/// Distinct id encoded in each crank's scheduled noop ix data and echoed in
/// `noop-fired:<id>` logs.
fn crank_fire_id(index: usize) -> u64 {
    (index as u64) + 1
}

fn crank_index_from_fire_id(id: u64) -> Result<usize> {
    if id == 0 || id > NUM_CRANKS as u64 {
        bail!("unexpected noop fire id {id}");
    }
    Ok((id - 1) as usize)
}

fn assert_crank_exists(rpc: &RpcClient, crank: &Pubkey) -> Result<()> {
    rpc.get_account(crank)
        .with_context(|| format!("get crank account {crank}"))?;
    Ok(())
}

#[derive(Clone)]
struct FireWatch {
    fires: Vec<Vec<Option<Duration>>>,
}

/// Background `logsSubscribe` on the noop program. Accumulates `noop-fired:<id>`
/// notifications until [`LogFireWatcher::wait_until`] returns.
struct LogFireWatcher {
    started: Instant,
    state: Arc<Mutex<FireWatch>>,
    shutdown: Arc<AtomicBool>,
    _sub: solana_pubsub_client::pubsub_client::PubsubLogsClientSubscription,
    thread: JoinHandle<()>,
}

impl LogFireWatcher {
    fn spawn(noop_id: Pubkey) -> Result<Self> {
        let ws_url = format!("ws://127.0.0.1:{}", ER_RPC_PORT + 1);
        let (sub, rx) = PubsubClient::logs_subscribe(
            &ws_url,
            RpcTransactionLogsFilter::Mentions(vec![noop_id.to_string()]),
            RpcTransactionLogsConfig {
                commitment: Some(CommitmentConfig::confirmed()),
            },
        )
        .context("logsSubscribe connect")?;
        eprintln!("[watch] logsSubscribe connected (noop {noop_id})");

        let started = Instant::now();
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(FireWatch {
            fires: vec![vec![None; TARGET_EXECUTIONS as usize]; NUM_CRANKS],
        }));
        let state_bg = state.clone();
        let shutdown_bg = shutdown.clone();
        let thread = thread::spawn(move || loop {
            if shutdown_bg.load(Ordering::Relaxed) {
                break;
            }
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(resp) => {
                    if resp.value.err.is_some() {
                        continue;
                    }
                    let mut guard = match state_bg.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    for line in &resp.value.logs {
                        let Some(id) = parse_noop_fired_log(line) else {
                            continue;
                        };
                        let Ok(idx) = crank_index_from_fire_id(id) else {
                            continue;
                        };

                        for fire in guard.fires[idx].iter_mut() {
                            if fire.is_none() {
                                eprintln!("[watch] crank {idx} fired at {:?}", started.elapsed());
                                *fire = Some(started.elapsed());
                                break;
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        });

        Ok(Self {
            started,
            state,
            shutdown,
            _sub: sub,
            thread,
        })
    }

    fn wait_until(self, deadline: Duration) -> Result<FireWatch> {
        while self.started.elapsed() < deadline {
            let done = {
                let guard = self.state.lock().expect("fire watch poisoned");
                guard.fires.iter().flatten().all(|fire| fire.is_some())
            };
            if done {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
        let watch = self.state.lock().expect("fire watch poisoned").clone();
        // PubsubClient::drop can block sending an unsubscribe over a closing socket.
        std::mem::forget(self._sub);
        Ok(watch)
    }
}

fn parse_noop_fired_log(line: &str) -> Option<u64> {
    let rest = line.split_once(NOOP_FIRED_PREFIX)?.1;
    rest.trim().parse().ok()
}

// --- RPC helpers ------------------------------------------------------------

/// Send + confirm a transaction with an explicit `fee_payer` and signer set.
///
/// Always `skip_preflight`: on the rollup, account cloning / delegation
/// adoption happens in the real send path (`chainlink::ensure_transaction_accounts`),
/// not in preflight simulation, so a simulation would wrongly reject the fee
/// payer. We poll the signature for the real outcome instead.
fn send(
    rpc: &RpcClient,
    ixs: &[Instruction],
    fee_payer: &Pubkey,
    signers: &[&Keypair],
) -> Result<()> {
    // The rollup clones referenced accounts from the base on first use; right
    // after startup that can transiently fail (`No clients provided for
    // Subscribe`) before its WS client pool is warm. These failures happen at
    // submission, before execution, so retrying is safe and idempotent.
    let mut last_err = None;
    for attempt in 0..8 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(1500));
        }
        match try_send(rpc, ixs, fee_payer, signers) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = format!("{e:#}");
                // A genuine on-chain revert won't get better with retries.
                if msg.contains("reverted") {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("send failed")))
}

fn try_send(
    rpc: &RpcClient,
    ixs: &[Instruction],
    fee_payer: &Pubkey,
    signers: &[&Keypair],
) -> Result<()> {
    use solana_rpc_client_api::config::RpcSendTransactionConfig;
    let bh = rpc.get_latest_blockhash().context("latest_blockhash")?;
    let msg = Message::new(ixs, Some(fee_payer));
    let tx = Transaction::new(signers, msg, bh);
    let sig = rpc
        .send_transaction_with_config(
            &tx,
            RpcSendTransactionConfig {
                skip_preflight: true,
                ..Default::default()
            },
        )
        .context("send_transaction")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rpc.get_signature_status(&sig)? {
            Some(Ok(())) => return Ok(()),
            Some(Err(e)) => bail!("tx {sig} reverted: {e:?}"),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(250)),
            None => bail!("tx {sig} not confirmed within 30s"),
        }
    }
}

/// Block until `rpc` reports a slot, or `timeout` elapses.
fn wait_for_rpc(rpc: &RpcClient, label: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last_err = String::new();
    while Instant::now() < deadline {
        match rpc.get_slot() {
            Ok(slot) if slot > 0 => {
                eprintln!("[stack] {label} healthy at slot {slot}");
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    bail!("{label} did not become healthy within {timeout:?}: {last_err}");
}

fn airdrop(rpc: &RpcClient, who: &Pubkey, lamports: u64) -> Result<()> {
    let sig = rpc
        .request_airdrop(who, lamports)
        .with_context(|| format!("airdrop to {who}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if rpc.confirm_transaction(&sig).unwrap_or(false) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    bail!("airdrop to {who} not confirmed in time");
}

/// MagicBlock delegation program (preloaded on the base by mb-test-validator).
fn delegation_program_id() -> Pubkey {
    Pubkey::from_str("DELeGGvXpWV2fqJUhqcF5ZSYMS4JTLjteaAMARRSaeSh").unwrap()
}
fn system_program_id() -> Pubkey {
    Pubkey::from_str("11111111111111111111111111111111").unwrap()
}
/// The ephemeral-validator's default identity (from its bundled keypair). The
/// fee-payer escrow must be delegated to this validator for the rollup to adopt
/// it. Logged at rollup startup as "Validator identity".
fn er_validator_identity() -> Pubkey {
    Pubkey::from_str("mAGicPQYBMvcYveUZA5F5UNNwyHvfYh5xkLS2Fr1mev").unwrap()
}

// --- Process orchestration --------------------------------------------------

/// Owns the spawned child processes and tears them down (SIGTERM, then SIGKILL)
/// on drop — including on panic, so a failing assertion never leaks validators.
struct Stack {
    children: Vec<(String, Child)>,
    tmp: TempDir,
}

impl Stack {
    fn push(&mut self, name: &str, child: Child) {
        self.children.push((name.to_string(), child));
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // Reverse order: cranker, then rollup, then base.
        for (name, child) in self.children.iter_mut().rev() {
            terminate(name, child);
        }
    }
}

/// Terminate the child's whole process group. The validators are launched via
/// npm wrappers that re-spawn the real binary as a grandchild and forward only
/// SIGINT — so a plain `kill <wrapper>` orphans the validator. Each child is
/// spawned as its own process-group leader (`process_group(0)`), so a negative
/// PID signals the wrapper *and* the grandchild. SIGINT first (graceful), then
/// SIGKILL.
fn terminate(name: &str, child: &mut Child) {
    let pid = child.id();
    let group = format!("-{pid}"); // negative PID = the process group
                                   // `2>/dev/null` equivalent: a group whose members already exited makes
                                   // `kill` print "No such process" — harmless noise we suppress.
    let signal = |sig: &str| {
        let _ = Command::new("kill")
            .arg(sig)
            .arg(&group)
            .stderr(Stdio::null())
            .status();
    };
    signal("-INT");
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => break,
        }
    }
    // Force-kill the whole group regardless, to be sure no grandchild lingers.
    signal("-KILL");
    let _ = child.kill();
    let _ = child.wait();
    let _ = name;
}

/// Spawn a child process, sending stdout/stderr to `log`, with `envs` set. The
/// child leads its own process group so [`terminate`] can signal the whole tree
/// (npm wrapper + the real validator binary it re-spawns).
fn spawn(program: &str, args: &[&str], log: fs::File, envs: &[(&str, &str)]) -> Result<Child> {
    use std::os::unix::process::CommandExt;
    let err_log = log.try_clone().context("clone log handle")?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log))
        .process_group(0);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            anyhow!("`{program}` not found on PATH — see this test's prerequisites")
        } else {
            anyhow!("failed to spawn `{program}`: {e}")
        }
    })
}

/// Print fee/escrow-relevant lines plus the tail of a log file (best effort).
fn dump_log(dir: &Path, name: &str) {
    const KEYWORDS: &[&str] = &[
        "fee", "escrow", "balance", "payer", "invalid", "delegat", "error", "warn", "clon",
    ];
    match fs::read_to_string(dir.join(name)) {
        Ok(s) => {
            let lines: Vec<&str> = s.lines().collect();
            let all_matched: Vec<&str> = lines
                .iter()
                .filter(|l| {
                    let low = l.to_lowercase();
                    KEYWORDS.iter().any(|k| low.contains(k))
                })
                .copied()
                .collect();
            let matched = &all_matched[all_matched.len().saturating_sub(40)..];
            let tail = &lines[lines.len().saturating_sub(15)..];
            eprintln!(
                "----- {name} (relevant) -----\n{}\n----- {name} (tail) -----\n{}\n-------------------------",
                matched.join("\n"),
                tail.join("\n")
            );
        }
        Err(e) => eprintln!("----- {name}: <unreadable: {e}> -----"),
    }
}

// --- Paths ------------------------------------------------------------------

/// Hydra workspace root (`tests/e2e` → `../..`).
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn artifact(rel: &str) -> Result<PathBuf> {
    let p = workspace_root().join(rel);
    if !p.exists() {
        bail!(
            "missing build artifact {} — see the prerequisites in this file's doc comment",
            p.display()
        );
    }
    Ok(p)
}

/// Build the `hydra-cranker` binary and return its path. The cranker is a
/// single binary that selects the ephemeral program at runtime (`--ephemeral`),
/// so no special feature is needed — we just ensure it's freshly built.
fn build_cranker() -> Result<PathBuf> {
    let root = workspace_root();
    eprintln!("[stack] building hydra-cranker");
    let status = Command::new(env!("CARGO"))
        .current_dir(&root)
        .args(["build", "-p", "hydra-cranker"])
        .status()
        .context("spawn cargo build for hydra-cranker")?;
    if !status.success() {
        bail!("cargo build -p hydra-cranker failed");
    }
    artifact("target/debug/hydra-cranker")
}

/// Write a keypair to a JSON byte-array file (the `--keypair` flag format).
fn write_keypair(dir: &Path, name: &str, kp: &Keypair) -> Result<PathBuf> {
    let path = dir.join(name);
    let bytes = kp.to_bytes();
    let json: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
    fs::write(&path, format!("[{}]", json.join(","))).context("write keypair file")?;
    Ok(path)
}

/// A self-deleting temp directory for validator ledgers/storage and keypairs.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<Self> {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("hydra-e2e-{nanos}"));
        fs::create_dir_all(&p).with_context(|| format!("create temp dir {}", p.display()))?;
        Ok(TempDir(p))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Keep the dir (logs, ledgers) for post-mortem when debugging.
        if std::env::var_os("KEEP_E2E_LOGS").is_some() {
            eprintln!("[stack] KEEP_E2E_LOGS set — leaving {}", self.0.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}
