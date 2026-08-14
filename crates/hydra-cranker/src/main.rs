//! Event-driven Hydra cranker.

use std::{
    collections::{HashMap, HashSet},
    io::Cursor,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use clap::Parser;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::{read_keypair, read_keypair_file, Keypair};
use solana_pubkey::Pubkey;
use solana_signer::Signer;

/// Consecutive failures at the same `next_exec_slot` before a crank is parked.
const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// Backstop floor between fires of the same crank, on top of the optimistic
/// `next_exec_slot` advance done on every successful submit (see
/// `cache::advance_after_trigger`). That advance is what normally gates re-fire
/// to the crank's own `interval_slots`; this only matters when it can't move the
/// schedule — `interval_slots == 0` ("every slot") cranks — where it caps the
/// blind retry rate at one fire per slot until the `programSubscribe` echo lands.
const POST_SUBMIT_COOLDOWN_SLOTS: u64 = 1;

/// Slots between Close attempts on the same crank. Close is one-shot: success
/// purges the crank from the cache, so this map only tracks race losers.
const CLOSE_RETRY_COOLDOWN_SLOTS: u64 = 10;

struct FailureState {
    count: u32,
    at_slot: u64,
    next_retry_slot: u64,
}

/// Slots between retries at `count` consecutive failures: first two are
/// adjacent slot ticks, then the gap doubles (2, 4, 8, …), capped.
fn retry_backoff_slots(count: u32) -> u64 {
    if count <= 2 {
        1
    } else {
        1u64 << (count - 2).min(10)
    }
}

mod cache;
mod delegation;
mod fire;
mod grpc;
mod metrics;
mod mode;
mod watch;

use cache::new_cache;

#[derive(Parser, Debug)]
#[command(
    name = "hydra-cranker",
    about = "Permissionless Hydra crank runner",
    version
)]
struct Cli {
    /// Solana JSON-RPC endpoint.
    #[arg(
        long,
        env = "HYDRA_CRANKER_RPC_URL",
        default_value = "https://api.devnet.solana.com"
    )]
    rpc_url: String,
    /// WebSocket endpoint. Derived from `--rpc-url` if omitted
    /// (`http`→`ws`, `https`→`wss`).
    #[arg(long, env = "HYDRA_CRANKER_WS_URL")]
    ws_url: Option<String>,
    /// Cranker keypair. Pays tx fees and receives the per-trigger reward.
    #[arg(long, env = "HYDRA_CRANKER_KEYPAIR")]
    keypair: String,
    /// If set, serve Prometheus metrics at `http://0.0.0.0:<port>/metrics`.
    #[arg(long, env = "HYDRA_CRANKER_PROMETHEUS_PORT")]
    prometheus_port: Option<u16>,
    /// Optional Yellowstone gRPC endpoint (e.g. `https://grpc.example:10000`).
    /// When set, a gRPC subscription runs **in addition to** the WS subs and
    /// feeds the same cache + slot tick channel — extra redundancy and
    /// usually lower latency than `programSubscribe` / `slotSubscribe`.
    #[arg(long, env = "HYDRA_CRANKER_GRPC_URL")]
    grpc_url: Option<String>,
    /// Optional `x-token` header for the gRPC endpoint.
    #[arg(long, env = "HYDRA_CRANKER_GRPC_X_TOKEN")]
    grpc_x_token: Option<String>,
    /// Priority fee, in micro-lamports per compute unit, attached to every
    /// trigger tx via `ComputeBudget::SetComputeUnitPrice`. `0` (default)
    /// omits the ix entirely — no cost, no tx-size overhead. Typical values
    /// under contention: 1_000 – 100_000.
    #[arg(
        long,
        env = "HYDRA_CRANKER_PRIORITY_FEE_MICRO_LAMPORTS",
        default_value_t = 0
    )]
    priority_fee_micro_lamports: u64,
    /// Send `Trigger` txs with `skip_preflight = true`. Off by default so
    /// preflight catches reverts before the leader charges fees. Turn on to
    /// surface failing inner ixs on-chain (otherwise they stall in simulation
    /// and never produce a signature, hiding the failure mode).
    #[arg(
        long,
        env = "HYDRA_CRANKER_TRIGGER_SKIP_PREFLIGHT",
        default_value_t = false
    )]
    trigger_skip_preflight: bool,
    /// Target Hydra's ephemeral-rollup program instead of the base-layer one.
    /// Switches the watched program ID, the `Close` account layout, and the
    /// funding/eligibility model (ephemeral cranks hold zero lamports). Point
    /// `--rpc-url` at a MagicBlock ephemeral validator.
    #[arg(long, env = "HYDRA_CRANKER_EPHEMERAL", default_value_t = false)]
    ephemeral: bool,
    /// Run *every* eligible crank, including ones whose scheduled instructions
    /// reference the cranker's own pubkey. Such cranks can't actually fire — as
    /// the fee payer the cranker is promoted to signer + writable, so the
    /// follow-up bytes never match the stored template — and would, if they
    /// could, hand a scheduled ix write access to the cranker's account. They
    /// are skipped by default; this flag opts back in.
    #[arg(long = "unsafe", env = "HYDRA_CRANKER_UNSAFE", default_value_t = false)]
    run_unsafe: bool,
    /// Base-layer (L1) JSON-RPC endpoint. Required with `--ephemeral`: the
    /// cranker keypair must be delegated for the rollup to let its balance
    /// change as the trigger fee payer, and delegating is a base-layer
    /// transaction. The cranker delegates itself here at startup if needed,
    /// paying for it out of its own balance. Unused in base mode.
    #[arg(long, env = "HYDRA_CRANKER_BASE_RPC_URL")]
    base_rpc_url: Option<String>,
    /// How long to wait, with `--ephemeral`, for the rollup to report the
    /// cranker keypair as delegated before giving up and exiting. The cranker
    /// polls `getDelegationStatus` for this long, covering the lag between the
    /// base-layer delegation landing and the rollup cloning the account.
    /// `0` checks exactly once.
    #[arg(
        long,
        env = "HYDRA_CRANKER_DELEGATION_TIMEOUT_SECS",
        default_value_t = 30
    )]
    delegation_timeout_secs: u64,
}

fn default_ws_url(rpc_url: &str) -> String {
    if let Some(r) = rpc_url.strip_prefix("https://") {
        format!("wss://{r}")
    } else if let Some(r) = rpc_url.strip_prefix("http://") {
        format!("ws://{r}")
    } else {
        // Unknown scheme — hand it to PubsubClient and let it error out.
        rpc_url.to_string()
    }
}

fn load_keypair(input: &str) -> Result<Keypair> {
    if Path::new(input).exists() {
        return read_keypair_file(input).map_err(|e| anyhow!("load keypair file {input}: {e}"));
    }

    let mut reader = Cursor::new(input);
    if let Ok(keypair) = read_keypair(&mut reader) {
        return Ok(keypair);
    }

    if let Ok(keypair) = Keypair::try_from_base58_string(input) {
        return Ok(keypair);
    }

    Err(anyhow!(
        "invalid keypair input: expected an existing file path, a JSON array with 64 bytes, or a base58-encoded keypair"
    ))
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Cli::parse();
    mode::init(args.ephemeral);
    let cranker = load_keypair(&args.keypair)?;
    let cranker_pubkey = cranker.pubkey();
    log::info!("cranker pubkey = {}", cranker_pubkey);
    log::info!(
        "mode = {}",
        if args.ephemeral { "ephemeral" } else { "base" }
    );
    if args.run_unsafe {
        log::warn!("--unsafe: running cranks that reference the cranker's own pubkey");
    }

    // Bootstrap must use the same commitment as `programSubscribe` or a
    // reconnect hands off a stale cache.
    let rpc = RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::processed());
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| default_ws_url(&args.rpc_url));
    log::info!("rpc = {}", args.rpc_url);
    log::info!("ws  = {}", ws_url);

    let program_id = mode::program_id();
    let cache = new_cache();
    let shutdown = Arc::new(AtomicBool::new(false));
    // `at_slot` anchors each counter to an observed `next_exec_slot`: once
    // the cache reports a newer one, the record is implicitly stale and the
    // crank is re-enabled.
    let mut failures: HashMap<Pubkey, FailureState> = HashMap::new();
    let mut last_submit: HashMap<Pubkey, u64> = HashMap::new();
    let mut last_close_attempt: HashMap<Pubkey, u64> = HashMap::new();
    let mut last_trigger_attempt_slot: Option<u64> = None;

    // Prometheus metrics endpoint (optional).
    if let Some(port) = args.prometheus_port {
        let _server = metrics::spawn_server(port);
    }

    // Ephemeral mode requires a delegated cranker: the rollup rejects a fee
    // payer whose lamports change unless it is delegated, and every `Trigger`
    // credits the cranker its reward. Fail fast here rather than let the
    // trigger loop park every crank on `InvalidAccountForFee`. Base mode moves
    // no fee-payer lamports this way and needs none of it.
    // Throwaway signer for the shutdown undelegate. It is never a fee payer and
    // never debited, so it needs no lamports and need not exist on-chain — its
    // only job is to be an *undelegated* instruction payer, which is what keeps
    // the magic program off its fee path. See `delegation::undelegate`.
    let undelegate_payer = Keypair::new();
    // Whether *this process* took the delegation, which is what entitles it to
    // release it on the way out. A pre-existing delegation belongs to whoever
    // made it — an operator, a sponsor tool, or a second cranker on the same
    // key — and tearing it down here would strand them on
    // `InvalidAccountForFee`.
    let mut delegated_by_us = false;

    if mode::is_ephemeral() {
        log::info!("undelegate payer = {}", undelegate_payer.pubkey());
        let url = args.base_rpc_url.clone().ok_or_else(|| {
            anyhow!("--base-rpc-url is required with --ephemeral (delegating the cranker keypair is a base-layer transaction)")
        })?;
        log::info!("base rpc = {}", url);
        let base = RpcClient::new_with_commitment(url, CommitmentConfig::confirmed());
        delegated_by_us = delegation::ensure_delegated(&base, &rpc, &cranker)?;
        // Landing on the base layer is not the same as the rollup having seen
        // it; the trigger loop needs the rollup's view.
        delegation::wait_until_delegated(
            &rpc,
            &cranker_pubkey,
            Duration::from_secs(args.delegation_timeout_secs),
        )?;
    } else if args.base_rpc_url.is_some() {
        log::warn!("--base-rpc-url is ignored without --ephemeral");
    }

    // Initial bootstrap so the trigger loop has something to scan even if
    // no WS notification arrives before the first slot tick.
    let n = watch::bootstrap(&rpc, &program_id, &cache)?;
    metrics::metrics().cranks_cached.set(n as i64);
    log::info!("bootstrap: {} crank(s) cached", n);

    let (slot_tx, slot_rx) = mpsc::channel::<u64>();
    let _program_thread = watch::spawn_program_watcher(
        args.rpc_url.clone(),
        ws_url.clone(),
        program_id,
        cache.clone(),
        shutdown.clone(),
    );
    let _slot_thread = watch::spawn_slot_watcher(
        args.rpc_url.clone(),
        ws_url,
        shutdown.clone(),
        slot_tx.clone(),
    );

    // Optional Yellowstone gRPC source. Strictly additive — feeds the same
    // cache and slot channel as the WS watchers, so whichever delivers an
    // update first wins and the other becomes a backstop.
    let _grpc_thread = args.grpc_url.as_ref().map(|url| {
        log::info!("grpc = {}", url);
        grpc::spawn_grpc_watcher(
            url.clone(),
            args.grpc_x_token.clone(),
            program_id,
            cache.clone(),
            shutdown.clone(),
            slot_tx,
        )
    });

    // Ctrl-C handling:
    //   1st  → set shutdown flag + log. Main loop + watchers observe it
    //          on their next timeout tick and exit gracefully.
    //   2nd+ → hard-exit. `PubsubClient::Drop` can hang trying to send an
    //          unsubscribe over a dead socket, so we don't rely on clean
    //          thread teardown for responsiveness.
    {
        let shutdown = shutdown.clone();
        let hits = Arc::new(AtomicUsize::new(0));
        ctrlc::set_handler(move || {
            let n = hits.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                log::info!("shutdown requested (Ctrl-C again to force-exit)");
                shutdown.store(true, Ordering::Relaxed);
            } else {
                log::warn!("force-exiting");
                std::process::exit(130); // conventional SIGINT exit code
            }
        })
        .ok();
    }

    // Trigger loop. `recv_timeout` so we observe the shutdown flag within
    // 500 ms even if slotSubscribe has gone quiet (dropped WS, idle RPC).
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }
        let (slot, slot_observed_at) = match slot_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(slot) => (slot, Instant::now()),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        metrics::metrics().current_slot.set(slot as i64);
        // Time the full sweep (scan + fire). `observe_duration` on drop.
        let _sweep = metrics::metrics().sweep_duration_seconds.start_timer();

        // Close takes precedence: its staleness arm can overlap
        // `is_eligible`, and a stuck crank should be cleaned up rather than
        // re-fired.
        let (eligible, closable, live_pubkeys): (Vec<_>, Vec<_>, HashSet<Pubkey>) = {
            let guard = cache.lock().expect("cache poisoned");
            let live: HashSet<Pubkey> = guard.keys().copied().collect();
            let mut elig = Vec::new();
            let mut clos = Vec::new();
            for entry in guard.values() {
                let closable_by_us =
                    !mode::is_ephemeral() || entry.close_reporter_allowed(&cranker_pubkey);
                if entry.is_closable(slot) && closable_by_us {
                    clos.push(entry.clone());
                } else if entry.is_eligible(slot) {
                    // A crank that references the cranker's own pubkey can never
                    // fire (the cranker is promoted to signer + writable as the
                    // fee payer) and is unsafe to run — skip unless `--unsafe`.
                    if !args.run_unsafe && entry.references_account(&cranker_pubkey) {
                        log::debug!(
                            "slot {}: skipping crank {} — references cranker pubkey (use --unsafe to run)",
                            slot,
                            entry.pubkey
                        );
                        continue;
                    }
                    elig.push(entry.clone());
                }
            }
            (elig, clos, live)
        };
        failures.retain(|pk, _| live_pubkeys.contains(pk));
        last_submit.retain(|pk, _| live_pubkeys.contains(pk));
        last_close_attempt.retain(|pk, _| live_pubkeys.contains(pk));
        let eligible_now = eligible.len();
        metrics::metrics().eligible_now.set(eligible_now as i64);

        let mut max_overdue_slots = 0;
        let mut parked_now = 0;
        let mut triggerable = Vec::new();
        for entry in eligible {
            max_overdue_slots = max_overdue_slots.max(slot.saturating_sub(entry.next_exec_slot));
            if let Some(&at) = last_submit.get(&entry.pubkey) {
                if slot < at + POST_SUBMIT_COOLDOWN_SLOTS {
                    continue;
                }
            }
            // Only skip when `at_slot` still matches: a fresh `next_exec_slot`
            // means the crank advanced and the prior failure record is stale.
            if let Some(state) = failures.get(&entry.pubkey) {
                if state.at_slot == entry.next_exec_slot {
                    if state.count >= MAX_CONSECUTIVE_FAILURES {
                        parked_now += 1;
                        continue;
                    }
                    if slot < state.next_retry_slot {
                        continue;
                    }
                }
            }
            triggerable.push(entry);
        }
        let triggerable_now = triggerable.len();

        for entry in triggerable {
            last_trigger_attempt_slot = Some(slot);
            match fire::fire_trigger(
                &rpc,
                &cranker,
                &entry,
                args.priority_fee_micro_lamports,
                args.trigger_skip_preflight,
            ) {
                Ok(signature) => {
                    log::info!(
                        "slot {}: triggered {} (tx {})",
                        slot,
                        entry.pubkey,
                        signature
                    );
                    metrics::metrics()
                        .triggers_submitted_total
                        .with_label_values(&["ok"])
                        .inc();
                    last_submit.insert(entry.pubkey, slot);
                    // Replay `Trigger`'s schedule advance in our cache now, so
                    // the crank's next fire follows its `interval_slots` instead
                    // of stalling until the `programSubscribe` echo catches up.
                    cache::advance_after_trigger(&cache, entry.pubkey, entry.next_exec_slot);
                    // Failure record clears only when the cache observes an
                    // advanced `next_exec_slot`; submit-Ok alone isn't proof
                    // the tx landed.
                }
                Err(f) => {
                    log::debug!("slot {}: trigger {} dropped: {:#}", slot, entry.pubkey, f);
                    metrics::metrics()
                        .triggers_submitted_total
                        .with_label_values(&["err"])
                        .inc();
                    let rec = failures.entry(entry.pubkey).or_insert(FailureState {
                        count: 0,
                        at_slot: entry.next_exec_slot,
                        next_retry_slot: 0,
                    });
                    if rec.at_slot != entry.next_exec_slot {
                        *rec = FailureState {
                            count: 1,
                            at_slot: entry.next_exec_slot,
                            next_retry_slot: slot + retry_backoff_slots(1),
                        };
                    } else {
                        rec.count = rec.count.saturating_add(1);
                        rec.next_retry_slot = slot + retry_backoff_slots(rec.count);
                        if rec.count == MAX_CONSECUTIVE_FAILURES {
                            log::warn!(
                                "parking crank {} after {} consecutive failures at slot {}: {:#}",
                                entry.pubkey,
                                rec.count,
                                entry.next_exec_slot,
                                f
                            );
                        }
                    }
                }
            }
        }
        metrics::update_health(metrics::HealthSnapshot::observed(
            slot,
            slot_observed_at,
            eligible_now,
            triggerable_now,
            parked_now,
            max_overdue_slots,
            last_trigger_attempt_slot,
        ));

        for entry in closable {
            if let Some(&at) = last_close_attempt.get(&entry.pubkey) {
                if slot < at + CLOSE_RETRY_COOLDOWN_SLOTS {
                    continue;
                }
            }
            match fire::fire_close(&rpc, &cranker, &entry, args.priority_fee_micro_lamports) {
                Ok(signature) => {
                    log::info!("slot {}: closed {} (tx {})", slot, entry.pubkey, signature);
                    metrics::metrics()
                        .closes_submitted_total
                        .with_label_values(&["ok"])
                        .inc();
                    last_close_attempt.insert(entry.pubkey, slot);
                }
                Err(f) => {
                    log::debug!("slot {}: close {} dropped: {:#}", slot, entry.pubkey, f);
                    metrics::metrics()
                        .closes_submitted_total
                        .with_label_values(&["err"])
                        .inc();
                    last_close_attempt.insert(entry.pubkey, slot);
                }
            }
        }
    }

    shutdown.store(true, Ordering::Relaxed);

    // Release the delegation on the way out, committing the rewards earned on
    // the rollup back to L1. Best-effort: triggering has already stopped, so a
    // failure costs nothing this run, and the next startup finds the account
    // still delegated and skips re-delegating.
    //
    // Only ours to release, though: a delegation that was already in place at
    // startup is someone else's, and undelegating it would break whoever is
    // relying on it.
    if mode::is_ephemeral() {
        if delegated_by_us {
            if let Err(e) = delegation::undelegate(&rpc, &cranker, &undelegate_payer) {
                log::warn!("undelegate on shutdown failed, cranker stays delegated: {e:#}");
            }
        } else {
            log::info!(
                "leaving cranker {cranker_pubkey} delegated: the delegation predates this process"
            );
        }
    }

    std::process::exit(0);
}
