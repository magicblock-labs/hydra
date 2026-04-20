//! WebSocket subscriptions that keep the [`Cache`] fresh and emit slot
//! ticks for the trigger loop.
//!
//! Two long-running threads:
//!
//! * **program watcher** — subscribes to `programSubscribe(HYDRA_ID)`, decodes
//!   each notification, and upserts or removes the matching cache entry
//!   (removed when the account was closed and its data is gone).
//! * **slot watcher** — subscribes to `slotSubscribe` and forwards the current
//!   slot number into an `mpsc::Sender<u64>` that the main thread reads.
//!
//! Both threads auto-reconnect on disconnect with a fixed 5 s backoff.
//! On reconnect the cache is re-bootstrapped via `getProgramAccounts` so
//! we don't silently drift.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

use crossbeam_channel::RecvTimeoutError;

use anyhow::{Context, Result};
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_pubsub_client::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};

use crate::cache::{apply_update, Cache, CacheOutcome, CrankEntry};
use crate::metrics;

/// One-shot cache rehydrate via `getProgramAccounts`. Called once at startup
/// and again on every WS reconnect to resync.
pub fn bootstrap(rpc: &RpcClient, program_id: &Pubkey, cache: &Cache) -> Result<usize> {
    let accounts = rpc.get_program_accounts(program_id).map_err(|e| {
        metrics::metrics()
            .rpc_errors_total
            .with_label_values(&["get_program_accounts"])
            .inc();
        anyhow::Error::new(e).context("getProgramAccounts bootstrap")
    })?;
    let mut guard = cache.lock().expect("cache poisoned");
    guard.clear();
    for (pk, acct) in &accounts {
        if let Some(e) = CrankEntry::from_raw(*pk, acct.lamports, &acct.data) {
            guard.insert(*pk, e);
        }
    }
    let n = guard.len();
    metrics::metrics().cranks_cached.set(n as i64);
    Ok(n)
}

/// Spawn the `programSubscribe` watcher. Returns immediately; the thread
/// owns the subscription and runs until `shutdown` is set.
pub fn spawn_program_watcher(
    rpc_url: String,
    ws_url: String,
    program_id: Pubkey,
    cache: Cache,
    shutdown: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !shutdown.load(Ordering::Relaxed) {
            // Rebuild the RPC handle each attempt in case the previous one
            // is wedged.
            let rpc = RpcClient::new(rpc_url.clone());
            if let Err(e) = bootstrap(&rpc, &program_id, &cache) {
                log::warn!("bootstrap failed: {:#}; retrying in 5s", e);
                thread::sleep(Duration::from_secs(5));
                continue;
            }
            metrics::metrics()
                .ws_reconnects_total
                .with_label_values(&["program"])
                .inc();
            if let Err(e) = run_program_watch(&ws_url, &program_id, &cache, &shutdown) {
                log::warn!("programSubscribe loop ended: {:#}; reconnecting in 5s", e);
                thread::sleep(Duration::from_secs(5));
            }
        }
    })
}

fn run_program_watch(
    ws_url: &str,
    program_id: &Pubkey,
    cache: &Cache,
    shutdown: &AtomicBool,
) -> Result<()> {
    let config = RpcProgramAccountsConfig {
        filters: None,
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: None,
            commitment: Some(CommitmentConfig::processed()),
            min_context_slot: None,
        },
        with_context: None,
        sort_results: None,
    };
    // Holding `_sub` alive keeps the subscription open; dropping it ends it.
    let (_sub, rx) = PubsubClient::program_subscribe(ws_url, program_id, Some(config))
        .context("programSubscribe connect")?;
    log::info!("programSubscribe connected");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            break Ok(());
        }
        // Short timeout so we can observe shutdown without hanging forever.
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(resp) => apply_account_notification(cache, resp),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("programSubscribe channel disconnected")
            }
        }
    }
}

fn apply_account_notification(
    cache: &Cache,
    resp: solana_rpc_client_api::response::Response<
        solana_rpc_client_api::response::RpcKeyedAccount,
    >,
) {
    let keyed = resp.value;
    let pk: Pubkey = match keyed.pubkey.parse() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("skip notification: bad pubkey {:?}: {}", keyed.pubkey, e);
            return;
        }
    };
    let lamports = keyed.account.lamports;
    let data = keyed.account.data.decode().unwrap_or_default();
    record_outcome(cache, pk, apply_update(cache, pk, lamports, &data));
}

/// Side-effects shared by every cache-update site: log, bump the
/// `cache_events_total` counter, and refresh the `cranks_cached` gauge.
pub(crate) fn record_outcome(cache: &Cache, pk: Pubkey, outcome: CacheOutcome) {
    let label = match outcome {
        CacheOutcome::Inserted => {
            log::debug!("cache: inserted {}", pk);
            Some("insert")
        }
        CacheOutcome::Updated => {
            log::debug!("cache: updated {}", pk);
            Some("update")
        }
        CacheOutcome::Removed => {
            log::debug!("cache: removed {}", pk);
            Some("remove")
        }
        CacheOutcome::Unchanged => None,
    };
    if let Some(label) = label {
        metrics::metrics()
            .cache_events_total
            .with_label_values(&[label])
            .inc();
    }
    let len = cache.lock().expect("cache poisoned").len();
    metrics::metrics().cranks_cached.set(len as i64);
}

/// Spawn the `slotSubscribe` watcher. Sends `slot` values over `tick` for
/// each new slot the RPC node observes. Reconnect loop mirrors the program
/// watcher.
pub fn spawn_slot_watcher(
    ws_url: String,
    shutdown: Arc<AtomicBool>,
    tick: mpsc::Sender<u64>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !shutdown.load(Ordering::Relaxed) {
            metrics::metrics()
                .ws_reconnects_total
                .with_label_values(&["slot"])
                .inc();
            if let Err(e) = run_slot_watch(&ws_url, &shutdown, &tick) {
                log::warn!("slotSubscribe loop ended: {:#}; reconnecting in 5s", e);
                thread::sleep(Duration::from_secs(5));
            }
        }
    })
}

fn run_slot_watch(ws_url: &str, shutdown: &AtomicBool, tick: &mpsc::Sender<u64>) -> Result<()> {
    let (_sub, rx) = PubsubClient::slot_subscribe(ws_url).context("slotSubscribe connect")?;
    log::info!("slotSubscribe connected");
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break Ok(());
        }
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(info) => {
                // If the receiver went away, we have nothing to do.
                if tick.send(info.slot).is_err() {
                    break Ok(());
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("slotSubscribe channel disconnected")
            }
        }
    }
}
