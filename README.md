<div align="center">
  <img src="./logo.png" width="168" alt="Hydra logo" />
  <h1>Hydra</h1>
  <p>Permissionless Solana crank for scheduling instructions with minimum overhead.</p>
</div>

## Packages

| Package | Description | Version | Docs |
|:--------|:------------|:--------|:-----|
| `hydra` | Pinocchio `no_std` on-chain program | `0.1.0` | [Overview](#overview) |
| `hydra-api` | Shared Rust types, builders, and CPI helpers | `0.1.0` | [Integrating Hydra](#integrating-hydra) |

## Overview

Hydra stores a scheduled instruction in a crank PDA and lets anyone trigger it
when the schedule is due.

Each trigger transaction has two instructions:

```text
ix[k]   = Hydra.Trigger
ix[k+1] = scheduled instruction
```

`Trigger` verifies `ix[k+1]` against the bytes stored in the crank account. If
the scheduled instruction fails, the whole transaction rolls back.

Key constraints:

- scheduled instructions run top-level, not via CPI
- scheduled instructions cannot require signer metas
- `Trigger` is top-level only

## Motivation

Hydra is not a general-purpose automation platform. It's a minimal runner
for **permissionless scheduled instructions** — oracle ticks, AMM pokes,
public `crank()` endpoints, settle / liquidation gates that accept any
signer. Other schedulers (Clockwork, Tuktuk, …) dispatch via CPI from
their own program; Hydra instead verifies the scheduled instruction
against an on-chain template at the top level and lets the runtime
execute it as a sibling ix. No CPI frame, no dispatch overhead.

The cranker submits a plain two-instruction transaction; `Trigger`
`memcmp`s `ix[k+1]` against the bytes stored on the crank PDA at
`Create` time (~60 CU), collects the reward, and advances state.
Solana transaction atomicity handles failure — if the scheduled
instruction reverts, the whole tx reverts and Hydra's payout / state
advance revert with it. The scheduled instruction itself runs top-level
and gets the full CU budget and stack depth.

## Compute Units

Measured with `logging` disabled:

| Instruction | Hydra CU |
|---|---:|
| `Create` | 3292 |
| `Trigger` | 464 |
| `Trigger` (reject: no follow-up) | 378 |
| `Cancel` | 128 |
| `Close` (reject: healthy) | 139 |
| `Close` (underfunded) | 150 |

Reproduce:

```sh
cargo build-sbf --manifest-path programs/hydra/Cargo.toml
cargo build-sbf --manifest-path tests/programs/noop/Cargo.toml
cargo test -p hydra-tests cu_table -- --ignored --nocapture
```

## Build

```sh
# Build the on-chain program.
cargo build-sbf --manifest-path programs/hydra/Cargo.toml

# Build the cranker.
cargo build -p hydra-cranker

# Run the test suite.
cargo test -p hydra-tests
```

## Integrating Hydra

Use `hydra-api` from clients or from your own on-chain program.

| Use case | Feature | API |
|---|---|---|
| Host-side client | `client` | `Instruction` builders |
| `solana-program` / Anchor CPI | `cpi-native` | `hydra_api::cpi::native::*` |
| Pinocchio CPI | `cpi-pinocchio` | `hydra_api::cpi::pinocchio::*` |

`Trigger` is not exposed as a CPI helper. It must be sent as a top-level
instruction.

Examples:

- `examples/native`
- `examples/anchor`
- `examples/pinocchio`

## Creating a Crank

```rust
use hydra_api::instruction::{self as ix, CreateArgs};

let seed = [0x42u8; 32];
let (crank, _bump) = ix::find_crank_pda(&seed);

let create = ix::create(
    payer_pubkey,
    crank,
    &CreateArgs {
        seed,
        authority: [0u8; 32],
        start_slot: 0,
        interval_slots: 400,
        remaining: 0,
        priority_tip: 2_500,
        cu_limit: 0, // 0 = cranker omits SetComputeUnitLimit; cap 1_400_000
        scheduled_program_id: memo::ID,
        scheduled_metas: &[],
        scheduled_data: b"tick",
    },
);
```

## Costs

A crank has two upfront costs and a small per-trigger fee:

| | Amount | What happens to it |
|---|---|---|
| **Rent deposit** | ~0.002 SOL | Locked while the crank lives, refunded on close |
| **Create tx fee** | 5,000 lamports | Standard Solana base fee |
| **Per trigger** | 10,000 lamports + `priority_tip` | Drawn from the crank's balance, paid to the cranker |

The rent deposit scales with the scheduled instruction's size — ~0.002 SOL
for a minimal ix, up to ~0.003 SOL with a handful of accounts and a bit of
data. **You get it back**: `Cancel` refunds 100% to the authority; `Close`
refunds everything minus a 10,000-lamport cleanup bounty (≈99.5 – 99.7% of
the deposit).

Fund future triggers by sending a `system_program::transfer` to the crank
PDA — typically in the same transaction as `Create` — sized to
`runs × (10,000 + priority_tip)`. If the crank runs out of lamports,
`Trigger` stops firing before it can touch the rent deposit, so that
deposit is always recoverable.

## Running the Cranker

The cranker is event-driven and uses WebSocket subscriptions for account and
slot updates. Optionally, a Yellowstone gRPC endpoint can be wired in
alongside the WS subs (`--grpc-url`) for redundancy and lower latency.

```sh
# Devnet
hydra-cranker --keypair ~/.config/solana/cranker.json

# Custom RPC / WebSocket endpoints
hydra-cranker \
  --keypair ~/.config/solana/cranker.json \
  --rpc-url https://your.rpc.example \
  --ws-url wss://your.rpc.example

# With Prometheus metrics at http://0.0.0.0:9100/metrics
hydra-cranker \
  --keypair ~/.config/solana/cranker.json \
  --prometheus-port 9100

# With a Yellowstone gRPC endpoint **in addition to** the WS subscriptions.
# Account + slot updates flow into the same cache and slot tick channel —
# whichever transport delivers first wins, the other is a redundant backstop.
hydra-cranker \
  --keypair ~/.config/solana/cranker.json \
  --grpc-url https://your.grpc.example:10000 \
  --grpc-x-token your-optional-x-token
```

### Metrics

When `--prometheus-port <PORT>` is set the cranker serves `/metrics` in
Prometheus text format on `0.0.0.0:<PORT>`. All series are namespaced
`hydra_cranker_*` and pre-initialised so `rate()` works from scrape 1.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `cranks_cached` | gauge | — | Cranks currently in the in-memory cache. |
| `current_slot` | gauge | — | Last slot observed from `slotSubscribe`. |
| `eligible_now` | gauge | — | Cranks eligible to trigger on the last slot tick. |
| `triggers_submitted_total` | counter | `result={ok,err}` | Triggers submitted. |
| `ws_reconnects_total` | counter | `source={program,slot}` | WS (re)connect attempts. |
| `grpc_reconnects_total` | counter | `source={program,slot}` | Yellowstone gRPC (re)connect attempts (only when `--grpc-url` is set). |
| `cache_events_total` | counter | `kind={insert,update,remove}` | Cache mutations driven by `programSubscribe`. |
| `sweep_duration_seconds` | histogram | — | Wall time per slot-tick sweep (scan + fire). Buckets target sub-10 ms. |
| `rpc_errors_total` | counter | `op={get_program_accounts,get_latest_blockhash,send_transaction}` | RPC call errors, by failing operation. |

Useful alerts:

- `increase(hydra_cranker_current_slot[1m]) < 100` — WS wedged.
- `hydra_cranker_cranks_cached == 0` and `hydra_cranker_ws_reconnects_total > 2` — not subscribed / flaky endpoint.
- `rate(hydra_cranker_triggers_submitted_total{result="err"}[5m]) / rate(hydra_cranker_triggers_submitted_total[5m]) > 0.5` — majority of triggers failing.
- `hydra_cranker_eligible_now > 0` for >30 s with no `rate(triggers_submitted_total[1m])` — have work, not doing it.
- `histogram_quantile(0.99, rate(hydra_cranker_sweep_duration_seconds_bucket[5m])) > 0.05` — sweep p99 > 50 ms, perf regression or cache bloat.
- `rate(hydra_cranker_rpc_errors_total[5m]) > 0.1` — RPC endpoint failing a notable fraction of calls.

## Instruction Reference

| Disc | Name | Accounts | Data |
|---:|---|---|---|
| 0 | `Create` | `payer(w,s), crank(w), system_program` | schedule payload |
| 1 | `Trigger` | `crank(w), cranker(w,s), instructions_sysvar` | none |
| 2 | `Cancel` | `authority(s), crank(w), recipient(w)` | none |
| 3 | `Close` | `reporter(s,w), crank(w), recipient(w)` | none |

To add lamports to a live crank, send a plain `system_program::transfer` to
the crank PDA — no dedicated instruction exists.

## Limits

- `Trigger` is top-level only
- scheduled instructions cannot include signer metas
- `MAX_ACCOUNTS = 32`
- `MAX_DATA_LEN = 1024`
- reward is fixed at `10_000` lamports plus the stored priority tip

## Releasing

`hydra-api` is the only crate published to crates.io (`hydra` is a program,
not a library; `hydra-cranker` / the examples are workspace-local).

Release flow:

1. Bump `[workspace.package] version` in the root `Cargo.toml` (e.g. `0.1.1`).
2. Commit + tag with a matching `vX.Y.Z` tag and push both.
3. Create a GitHub release from that tag.

`.github/workflows/release.yml` triggers on `release: published`, verifies
the tag matches `hydra-api`'s manifest version, dry-runs the package, then
`cargo publish -p hydra-api`. Requires a `CARGO_REGISTRY_TOKEN` repo
secret (a crates.io API token scoped to publish-new + publish-update).

## License

MIT
