//! Prometheus metrics + `/metrics` HTTP endpoint.
//!
//! Minimal on purpose: five metrics, a `OnceLock` holding the registry,
//! and a blocking `tiny_http` server thread. No async runtime, no middleware.
//!
//! All metrics are namespaced `hydra_cranker_*`. Scrape with any Prometheus
//! (`curl http://host:PORT/metrics` for a sanity check).

use std::{sync::OnceLock, thread, time::Duration};

use prometheus::{
    register_histogram_with_registry, register_int_counter_vec_with_registry,
    register_int_gauge_with_registry, Encoder, Histogram, IntCounterVec, IntGauge, Registry,
    TextEncoder,
};

pub struct Metrics {
    pub registry: Registry,

    /// # cranks currently in the in-memory cache. Alert if this goes to 0
    /// after being non-zero without a clean shutdown.
    pub cranks_cached: IntGauge,

    /// Last slot number observed from `slotSubscribe`. If this stops
    /// advancing, the WS is wedged.
    pub current_slot: IntGauge,

    /// Number of trigger transactions submitted. `result=ok|err`.
    pub triggers_submitted_total: IntCounterVec,

    /// Number of permissionless `Close` transactions submitted. `result=ok|err`.
    pub closes_submitted_total: IntCounterVec,

    /// WS reconnect events. `source=program|slot`.
    pub ws_reconnects_total: IntCounterVec,

    /// Yellowstone gRPC reconnect events. `source=program|slot` mirrors
    /// `ws_reconnects_total` so the two transports are directly comparable.
    pub grpc_reconnects_total: IntCounterVec,

    /// Cache mutations driven by `programSubscribe`.
    /// `kind=insert|update|remove`.
    pub cache_events_total: IntCounterVec,

    /// Cranks that were eligible on the most recent slot tick (ready to
    /// trigger). Point-in-time snapshot, overwritten every scan.
    pub eligible_now: IntGauge,

    /// Duration of each slot-tick sweep: scan cache + fire all eligible.
    /// Custom fine-grained buckets targeted at the <10 ms healthy range.
    pub sweep_duration_seconds: Histogram,

    /// RPC call errors, labeled by the failing operation.
    /// `op={get_program_accounts,get_latest_blockhash,send_transaction}`.
    pub rpc_errors_total: IntCounterVec,
}

impl Metrics {
    fn new() -> Self {
        let registry = Registry::new_custom(Some("hydra_cranker".into()), None).expect("registry");
        let cranks_cached = register_int_gauge_with_registry!(
            "cranks_cached",
            "Number of cranks currently held in the in-memory cache.",
            registry
        )
        .unwrap();
        let current_slot = register_int_gauge_with_registry!(
            "current_slot",
            "Last slot observed from `slotSubscribe`.",
            registry
        )
        .unwrap();
        let triggers_submitted_total = register_int_counter_vec_with_registry!(
            "triggers_submitted_total",
            "Total triggers submitted, by outcome.",
            &["result"],
            registry
        )
        .unwrap();
        let closes_submitted_total = register_int_counter_vec_with_registry!(
            "closes_submitted_total",
            "Total permissionless Close txs submitted, by outcome.",
            &["result"],
            registry
        )
        .unwrap();
        let ws_reconnects_total = register_int_counter_vec_with_registry!(
            "ws_reconnects_total",
            "WebSocket (re)connect attempts, by source.",
            &["source"],
            registry
        )
        .unwrap();
        let grpc_reconnects_total = register_int_counter_vec_with_registry!(
            "grpc_reconnects_total",
            "Yellowstone gRPC (re)connect attempts, by source.",
            &["source"],
            registry
        )
        .unwrap();
        let cache_events_total = register_int_counter_vec_with_registry!(
            "cache_events_total",
            "programSubscribe-driven cache mutations, by kind.",
            &["kind"],
            registry
        )
        .unwrap();
        let eligible_now = register_int_gauge_with_registry!(
            "eligible_now",
            "Cranks eligible to trigger on the most recent slot tick.",
            registry
        )
        .unwrap();
        // Fine-grained buckets targeted at the healthy sub-10ms range; upper
        // buckets catch pathological sweeps (stuck lock, bursty RPC).
        let sweep_duration_seconds = register_histogram_with_registry!(
            prometheus::HistogramOpts::new(
                "sweep_duration_seconds",
                "Wall time per slot-tick sweep (cache scan + fire triggers)."
            )
            .buckets(vec![
                0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.5, 1.0, 5.0,
            ]),
            registry
        )
        .unwrap();
        let rpc_errors_total = register_int_counter_vec_with_registry!(
            "rpc_errors_total",
            "RPC call errors, by failing operation.",
            &["op"],
            registry
        )
        .unwrap();

        // Pre-init known label combinations so `rate()` queries don't get
        // "no data" before the first increment. Prometheus only materialises
        // a label series on first observation otherwise.
        for result in ["ok", "err"] {
            triggers_submitted_total
                .with_label_values(&[result])
                .inc_by(0);
            closes_submitted_total
                .with_label_values(&[result])
                .inc_by(0);
        }
        for source in ["program", "slot"] {
            ws_reconnects_total.with_label_values(&[source]).inc_by(0);
            grpc_reconnects_total.with_label_values(&[source]).inc_by(0);
        }
        for kind in ["insert", "update", "remove"] {
            cache_events_total.with_label_values(&[kind]).inc_by(0);
        }
        for op in [
            "get_program_accounts",
            "get_latest_blockhash",
            "send_transaction",
        ] {
            rpc_errors_total.with_label_values(&[op]).inc_by(0);
        }

        Self {
            registry,
            cranks_cached,
            current_slot,
            triggers_submitted_total,
            closes_submitted_total,
            ws_reconnects_total,
            grpc_reconnects_total,
            cache_events_total,
            eligible_now,
            sweep_duration_seconds,
            rpc_errors_total,
        }
    }
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn metrics() -> &'static Metrics {
    METRICS.get_or_init(Metrics::new)
}

/// Spawn a blocking HTTP server serving `GET /metrics` in text format.
/// The server thread runs for the lifetime of the process; on any bind
/// failure it logs and exits the thread (the cranker stays up).
pub fn spawn_server(port: u16) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let addr = format!("0.0.0.0:{port}");
        let server = match tiny_http::Server::http(&addr) {
            Ok(s) => s,
            Err(e) => {
                log::error!("metrics: failed to bind {addr}: {e}");
                return;
            }
        };
        log::info!("metrics server listening on {addr}/metrics");
        let metrics = metrics();
        let encoder = TextEncoder::new();
        // 1s poll so a clean process exit can proceed within bounded time.
        loop {
            match server.recv_timeout(Duration::from_secs(1)) {
                Ok(Some(req)) => {
                    let mut body = Vec::with_capacity(1024);
                    if let Err(e) = encoder.encode(&metrics.registry.gather(), &mut body) {
                        log::warn!("metrics encode error: {e}");
                        continue;
                    }
                    let resp = tiny_http::Response::from_data(body).with_header(
                        "Content-Type: text/plain; version=0.0.4"
                            .parse::<tiny_http::Header>()
                            .unwrap(),
                    );
                    let _ = req.respond(resp);
                }
                Ok(None) => {} // timeout, just loop
                Err(e) => {
                    log::warn!("metrics server recv error: {e}");
                    return;
                }
            }
        }
    })
}
