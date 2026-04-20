//! Optional Yellowstone gRPC streaming source.
//!
//! Runs alongside the WS subscriptions in [`crate::watch`] (not in place of
//! them): account updates flow into the same [`Cache`] and slot updates into
//! the same `mpsc::Sender<u64>`. Whichever transport delivers first wins;
//! the other becomes a redundant backstop. If `--grpc-url` is set the user
//! is opting in to that redundancy/lower latency.
//!
//! One dedicated OS thread runs a single-threaded tokio runtime that owns
//! the gRPC subscription. On disconnect it reconnects with a 5 s backoff,
//! mirroring the WS watchers.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use futures::{sink::SinkExt, stream::StreamExt};
use solana_pubkey::Pubkey;
use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts, SubscribeRequestFilterSlots,
};

use crate::{
    cache::{apply_update, Cache},
    metrics,
    watch::record_outcome,
};

/// Spawn the gRPC watcher. Returns immediately; the thread owns its tokio
/// runtime and the active subscription, and runs until `shutdown` is set.
pub fn spawn_grpc_watcher(
    endpoint: String,
    x_token: Option<String>,
    program_id: Pubkey,
    cache: Cache,
    shutdown: Arc<AtomicBool>,
    tick: mpsc::Sender<u64>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("hydra-grpc".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log::error!("grpc: failed to build tokio runtime: {e}");
                    return;
                }
            };
            rt.block_on(grpc_main(
                endpoint, x_token, program_id, cache, shutdown, tick,
            ));
        })
        .expect("spawn grpc thread")
}

async fn grpc_main(
    endpoint: String,
    x_token: Option<String>,
    program_id: Pubkey,
    cache: Cache,
    shutdown: Arc<AtomicBool>,
    tick: mpsc::Sender<u64>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        // Bump on every (re)connect attempt so the metric counts attempts,
        // not just successes — same convention as `ws_reconnects_total`.
        metrics::metrics()
            .grpc_reconnects_total
            .with_label_values(&["program"])
            .inc();
        metrics::metrics()
            .grpc_reconnects_total
            .with_label_values(&["slot"])
            .inc();
        match run_once(
            &endpoint,
            x_token.as_deref(),
            &program_id,
            &cache,
            &shutdown,
            &tick,
        )
        .await
        {
            Ok(()) => {
                // Clean shutdown.
                break;
            }
            Err(e) => {
                log::warn!("grpc subscribe ended: {e:#}; reconnecting in 5s");
                // Sleep in slices so shutdown is observed promptly.
                for _ in 0..50 {
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

async fn run_once(
    endpoint: &str,
    x_token: Option<&str>,
    program_id: &Pubkey,
    cache: &Cache,
    shutdown: &AtomicBool,
    tick: &mpsc::Sender<u64>,
) -> Result<()> {
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.to_string())
        .context("grpc: invalid endpoint")?;
    if let Some(token) = x_token {
        builder = builder
            .x_token(Some(token.to_string()))
            .context("grpc: invalid x-token")?;
    }
    let mut client = builder.connect().await.context("grpc: connect")?;
    log::info!("grpc connected: {endpoint}");

    let mut accounts = std::collections::HashMap::new();
    accounts.insert(
        "hydra".to_string(),
        SubscribeRequestFilterAccounts {
            account: vec![],
            owner: vec![program_id.to_string()],
            filters: vec![],
            ..Default::default()
        },
    );
    let mut slots = std::collections::HashMap::new();
    slots.insert(
        "slots".to_string(),
        SubscribeRequestFilterSlots {
            filter_by_commitment: Some(true),
            ..Default::default()
        },
    );
    let request = SubscribeRequest {
        accounts,
        slots,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };

    let (mut sink, mut stream) = client
        .subscribe_with_request(Some(request))
        .await
        .context("grpc: subscribe")?;
    // Best-effort drop of the request sender — we don't push further updates,
    // but keeping the half open avoids server-side close on some impls.
    let _ = sink.flush().await;

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
        let next = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => return Err(anyhow!("grpc stream closed")),
            // No traffic for 10 s is fine on idle programs; check shutdown
            // and keep waiting.
            Err(_) => continue,
        };
        let update = next.context("grpc: stream item")?;
        match update.update_oneof {
            Some(UpdateOneof::Account(acc)) => {
                let Some(info) = acc.account else { continue };
                let Ok(pk) = Pubkey::try_from(info.pubkey.as_slice()) else {
                    log::warn!(
                        "grpc: skip account with bad pubkey ({} bytes)",
                        info.pubkey.len()
                    );
                    continue;
                };
                let outcome = apply_update(cache, pk, info.lamports, &info.data);
                record_outcome(cache, pk, outcome);
            }
            Some(UpdateOneof::Slot(slot)) => {
                metrics::metrics().current_slot.set(slot.slot as i64);
                if tick.send(slot.slot).is_err() {
                    return Ok(());
                }
            }
            // Ping/Pong/etc — server-side liveness, ignore.
            _ => {}
        }
    }
}
