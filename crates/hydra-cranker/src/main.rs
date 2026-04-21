//! Event-driven Hydra cranker.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Result};
use clap::Parser;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::read_keypair_file;
use solana_pubkey::Pubkey;
use solana_signer::Signer;

/// Consecutive failures at the same `next_exec_slot` before a crank is parked.
const MAX_CONSECUTIVE_FAILURES: u32 = 10;

/// Slots to skip a crank after a successful submit. Absorbs the in-flight
/// window where both our cache and the RPC's preflight bank are stale; without
/// it, a second fire within the window lands as `NotYetExecutable` (0x1).
const POST_SUBMIT_COOLDOWN_SLOTS: u64 = 3;

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
mod fire;
mod grpc;
mod metrics;
mod watch;

use cache::new_cache;
use hydra_api::instruction as ix;

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
    keypair: PathBuf,
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Cli::parse();
    let cranker = read_keypair_file(&args.keypair)
        .map_err(|e| anyhow!("load keypair {}: {}", args.keypair.display(), e))?;
    log::info!("cranker pubkey = {}", cranker.pubkey());

    // Bootstrap must use the same commitment as `programSubscribe` or a
    // reconnect hands off a stale cache.
    let rpc = RpcClient::new_with_commitment(args.rpc_url.clone(), CommitmentConfig::processed());
    let ws_url = args
        .ws_url
        .clone()
        .unwrap_or_else(|| default_ws_url(&args.rpc_url));
    log::info!("rpc = {}", args.rpc_url);
    log::info!("ws  = {}", ws_url);

    let program_id = ix::program_id();
    let cache = new_cache();
    let shutdown = Arc::new(AtomicBool::new(false));
    // `at_slot` anchors each counter to an observed `next_exec_slot`: once
    // the cache reports a newer one, the record is implicitly stale and the
    // crank is re-enabled.
    let mut failures: HashMap<Pubkey, FailureState> = HashMap::new();
    let mut last_submit: HashMap<Pubkey, u64> = HashMap::new();
    let mut last_close_attempt: HashMap<Pubkey, u64> = HashMap::new();

    // Prometheus metrics endpoint (optional).
    if let Some(port) = args.prometheus_port {
        let _server = metrics::spawn_server(port);
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
    let _slot_thread = watch::spawn_slot_watcher(ws_url, shutdown.clone(), slot_tx.clone());

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
        let slot = match slot_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(slot) => slot,
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
                if entry.is_closable(slot) {
                    clos.push(entry.clone());
                } else if entry.is_eligible(slot) {
                    elig.push(entry.clone());
                }
            }
            (elig, clos, live)
        };
        failures.retain(|pk, _| live_pubkeys.contains(pk));
        last_submit.retain(|pk, _| live_pubkeys.contains(pk));
        last_close_attempt.retain(|pk, _| live_pubkeys.contains(pk));
        metrics::metrics().eligible_now.set(eligible.len() as i64);

        for entry in eligible {
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
                        continue;
                    }
                    if slot < state.next_retry_slot {
                        continue;
                    }
                }
            }
            match fire::fire_trigger(&rpc, &cranker, &entry, args.priority_fee_micro_lamports) {
                Ok(()) => {
                    log::info!("slot {}: triggered {}", slot, entry.pubkey);
                    metrics::metrics()
                        .triggers_submitted_total
                        .with_label_values(&["ok"])
                        .inc();
                    last_submit.insert(entry.pubkey, slot);
                    // Failure record clears only when the cache observes an
                    // advanced `next_exec_slot`; submit-Ok alone isn't proof
                    // the tx landed.
                }
                Err(e) => {
                    log::debug!("slot {}: trigger {} dropped: {:#}", slot, entry.pubkey, e);
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
                                e
                            );
                        }
                    }
                }
            }
        }

        for entry in closable {
            if let Some(&at) = last_close_attempt.get(&entry.pubkey) {
                if slot < at + CLOSE_RETRY_COOLDOWN_SLOTS {
                    continue;
                }
            }
            match fire::fire_close(&rpc, &cranker, &entry, args.priority_fee_micro_lamports) {
                Ok(()) => {
                    log::info!("slot {}: closed {}", slot, entry.pubkey);
                    metrics::metrics()
                        .closes_submitted_total
                        .with_label_values(&["ok"])
                        .inc();
                }
                Err(e) => {
                    log::debug!("slot {}: close {} dropped: {:#}", slot, entry.pubkey, e);
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
    std::process::exit(0);
}
