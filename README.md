<div align="center">
  <img src="./logo.png" width="168" alt="Hydra logo" />
  <h1>Hydra</h1>
  <p>Permissionless Solana crank for scheduling instructions with minimum overhead.</p>
</div>

## Packages

| Package     | Description                                  | Version | Docs                                    |
| :---------- | :------------------------------------------- | :------ | :-------------------------------------- |
| `hydra`     | Pinocchio `no_std` on-chain program          | `0.1.0` | [Overview](#overview)                   |
| `hydra-api` | Shared Rust types, builders, and CPI helpers | `0.1.0` | [Integrating Hydra](#integrating-hydra) |

## Overview

Hydra stores one or more scheduled instructions in a crank PDA and lets anyone
trigger them when the schedule is due.

Each trigger transaction places the scheduled instructions immediately after
`Trigger`:

```text
ix[k]     = Hydra.Trigger
ix[k+1]   = scheduled instruction 1
ix[k+2]   = scheduled instruction 2
…
ix[k+n]   = scheduled instruction n
```

`Trigger` verifies `ix[k+1..=k+n]` against the bytes stored in the crank
account. Because the instructions sysvar lays instruction blobs out
contiguously, this verification is a single `memcmp` regardless of `n`. If any
scheduled instruction fails, the whole transaction rolls back.

Key constraints:

- scheduled instructions run top-level, not via CPI
- scheduled instructions cannot require signer metas
- the scheduled instructions must be contiguous and in order, right after `Trigger`
- a crank holds at most `MAX_INSTRUCTIONS` (16) scheduled instructions
- `Trigger` is top-level only

## Motivation

Hydra is not a general-purpose automation platform. It's a minimal runner
for **permissionless scheduled instructions** — oracle ticks, AMM pokes,
public `crank()` endpoints, settle / liquidation gates that accept any
signer. Other schedulers (Clockwork, Tuktuk, …) dispatch via CPI from
their own program; Hydra instead verifies the scheduled instruction
against an on-chain template at the top level and lets the runtime
execute it as a sibling ix. No CPI frame, no dispatch overhead.

The cranker submits a plain transaction (`Trigger` followed by the scheduled
instructions); `Trigger` `memcmp`s `ix[k+1..]` against the bytes stored on the
crank PDA at `Create` time (~60 CU), collects the reward, and advances state.
The reward and the schedule advance are flat per `Trigger`, independent of how
many instructions the crank holds. Solana transaction atomicity handles failure
— if any scheduled instruction reverts, the whole tx reverts and Hydra's payout
/ state advance revert with it. The scheduled instructions themselves run
top-level and get the full CU budget and stack depth.

## Compute Units

Measured with `logging` disabled:

| Instruction                      | Hydra CU |
| -------------------------------- | -------: |
| `Create`                         |     5634 |
| `Trigger` (happy, 1 sibling)     |      466 |
| `Trigger` (happy, 3 siblings)    |      466 |
| `Trigger` (reject: no follow-up) |      379 |
| `Cancel`                         |      141 |
| `Close` (reject: healthy)        |      270 |
| `Close` (underfunded)            |      300 |

`Trigger` costs the same whether the crank schedules one instruction or many —
the single concatenated `memcmp` is the entire verification, so adding
instructions adds no Hydra-side CU. `Create` scales with the total scheduled
payload size (it is a one-time cost dominated by the account-creation syscall).

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

| Use case                      | Feature         | API                            |
| ----------------------------- | --------------- | ------------------------------ |
| Host-side client              | `client`        | `Instruction` builders         |
| `solana-program` / Anchor CPI | `cpi-native`    | `hydra_api::cpi::native::*`    |
| Pinocchio CPI                 | `cpi-pinocchio` | `hydra_api::cpi::pinocchio::*` |

`Trigger` is not exposed as a CPI helper. It must be sent as a top-level
instruction.

Examples:

- `examples/native`
- `examples/anchor`
- `examples/pinocchio`

## Creating a Crank

```rust
use hydra_api::instruction::{self as ix, CreateArgs, ScheduledIx};

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
        // One or more scheduled ixs, run top-level in order after `Trigger`.
        scheduled: &[ScheduledIx {
            program_id: memo::ID,
            metas: &[],
            data: b"tick",
        }],
    },
);
```

## Authenticating a Crank PDA

Scheduled instructions run top-level, so a target program cannot rely on Hydra
CPI signer privileges. If the scheduled ix needs to authenticate a Hydra crank,
include the crank PDA and instructions sysvar in the scheduled ix and verify
the sibling instructions in both directions.

Hydra does the forward check: `Trigger` reads the instructions sysvar and
requires `ix[k+1]` to byte-match the scheduled ix stored in the crank PDA. The
scheduled program can do the reverse check: read the current instruction index,
load `ix[k-1]`, and require it to be Hydra `Trigger` for the same crank PDA.

```text
ix[k-1] = Hydra.Trigger(crank = expected_crank_pda, ...)
ix[k]   = your scheduled ix(crank = expected_crank_pda, instructions_sysvar, ...)
```

In the scheduled program, reject unless:

- `expected_crank_pda == Pubkey::find_program_address([b"crank", seed], hydra_id)`
- `crank.owner == hydra_id`
- the previous ix program id is `hydra_id`
- the previous ix discriminator is `Trigger`
- the previous ix first account is the same crank PDA

If the scheduled ix also needs to verify who created the schedule, read the
crank header, for example with `hydra_api::state::load_crank`, and check both
`authority` and `authority_signer`. `authority` is the value supplied at
`Create`; `authority_signer == 1` means the `Create` payer/signer was that
same authority. Require `authority == expected_authority` and
`authority_signer == 1` when scheduler identity matters. If
`authority_signer == 0`, the authority is only stored for cancellation and is
not proof that the authority signed the schedule creation.

## Costs

A crank has two upfront costs and a small per-trigger fee:

|                   | Amount                           | What happens to it                                  |
| ----------------- | -------------------------------- | --------------------------------------------------- |
| **Rent deposit**  | ~0.002 SOL                       | Locked while the crank lives, refunded on close     |
| **Create tx fee** | 5,000 lamports                   | Standard Solana base fee                            |
| **Per trigger**   | 10,000 lamports + `priority_tip` | Drawn from the crank's balance, paid to the cranker |

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

# Against a MagicBlock ephemeral rollup. `--ephemeral` switches the target
# program, the `Close` account layout, and the (zero-lamport) funding model at
# runtime — the same binary drives either program, no rebuild needed.
hydra-cranker \
  --keypair ~/.config/solana/cranker.json \
  --rpc-url https://your.rollup.example \
  --ephemeral

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

| Metric                     | Type      | Labels                                                            | Meaning                                                                |
| -------------------------- | --------- | ----------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `cranks_cached`            | gauge     | —                                                                 | Cranks currently in the in-memory cache.                               |
| `current_slot`             | gauge     | —                                                                 | Last slot observed from `slotSubscribe`.                               |
| `eligible_now`             | gauge     | —                                                                 | Cranks eligible to trigger on the last slot tick.                      |
| `triggers_submitted_total` | counter   | `result={ok,err}`                                                 | Triggers submitted.                                                    |
| `ws_reconnects_total`      | counter   | `source={program,slot}`                                           | WS (re)connect attempts.                                               |
| `grpc_reconnects_total`    | counter   | `source={program,slot}`                                           | Yellowstone gRPC (re)connect attempts (only when `--grpc-url` is set). |
| `cache_events_total`       | counter   | `kind={insert,update,remove}`                                     | Cache mutations driven by `programSubscribe`.                          |
| `sweep_duration_seconds`   | histogram | —                                                                 | Wall time per slot-tick sweep (scan + fire). Buckets target sub-10 ms. |
| `rpc_errors_total`         | counter   | `op={get_program_accounts,get_latest_blockhash,send_transaction}` | RPC call errors, by failing operation.                                 |

Useful alerts:

- `increase(hydra_cranker_current_slot[1m]) < 100` — WS wedged.
- `hydra_cranker_cranks_cached == 0` and `hydra_cranker_ws_reconnects_total > 2` — not subscribed / flaky endpoint.
- `rate(hydra_cranker_triggers_submitted_total{result="err"}[5m]) / rate(hydra_cranker_triggers_submitted_total[5m]) > 0.5` — majority of triggers failing.
- `hydra_cranker_eligible_now > 0` for >30 s with no `rate(triggers_submitted_total[1m])` — have work, not doing it.
- `histogram_quantile(0.99, rate(hydra_cranker_sweep_duration_seconds_bucket[5m])) > 0.05` — sweep p99 > 50 ms, perf regression or cache bloat.
- `rate(hydra_cranker_rpc_errors_total[5m]) > 0.1` — RPC endpoint failing a notable fraction of calls.

## Instruction Reference

Discriminators `0–3` are the base-layer crank. Discriminators `4–7` are the
[ephemeral-rollup crank](#ephemeral-rollup-crank-feature-ephemeral), compiled
only under the `ephemeral` feature.

| Disc | Name      | Accounts                                      | Data             |
| ---: | --------- | --------------------------------------------- | ---------------- |
|    0 | `Create`  | `payer(w,s), crank(w), system_program`        | schedule payload |
|    1 | `Trigger` | `crank(w), cranker(w,s), instructions_sysvar` | none             |
|    2 | `Cancel`  | `authority(s), crank(w), recipient(w)`        | none             |
|    3 | `Close`   | `reporter(s,w), crank(w), recipient(w)`       | none             |

To add lamports to a live crank, send a plain `system_program::transfer` to
the crank PDA — no dedicated instruction exists.

## Limits

- `Trigger` is top-level only
- scheduled instructions cannot include signer metas
- `MAX_ACCOUNTS = 32`
- `MAX_DATA_LEN = 1024`
- reward is fixed at `10_000` lamports plus the stored priority tip

## Ephemeral Rollup Crank (feature `ephemeral`)

Behind the optional `ephemeral` cargo feature, Hydra exposes a parallel set of
instructions that run a crank on a [MagicBlock](https://magicblock.gg) **ephemeral
rollup (ER)**, where the crank lives as a MagicBlock **ephemeral account** instead
of a base-layer PDA. The default (mainnet) build neither compiles this code nor
pulls its dependency (`ephemeral-rollups-pinocchio`).

The model is the same as the base crank — a crank stores scheduled instructions,
anyone triggers them when due, and `Trigger` verifies the follow-up siblings with
the same single `memcmp` — with two differences the ER forces:

- **Ephemeral accounts hold zero lamports.** Rent (a flat per-byte fee) is paid by
  a sponsor into a shared vault, not held in the account. So there is no cranker
  reward, no priority-tip payout, and no rent floor: `TriggerEphemeral` only
  verifies the follow-up ix and advances the schedule (slot check, decrement
  `remaining`, bump `executed`).
- **Creation is a single instruction.** The Magic program materializes the
  ephemeral account synchronously, so `CreateEphemeral` allocates the crank (via a
  Magic CPI signed by the crank PDA) and writes its header + scheduled-ix tail in
  one instruction — no separate init step.

Lifecycle: `CreateEphemeral` → `TriggerEphemeral` (+ scheduled siblings, run
top-level on the ER) → `CancelEphemeral` (authority-gated) / `CloseEphemeral`
(permissionless, when the crank is exhausted or stuck). `Cancel`/`Close` CPI the
Magic program to close the ephemeral account and refund the vault rent to the
sponsor; if a non-zero `authority` is set, only that authority may close it. The
crank PDA derivation (`[b"crank", seed]`) and the on-chain `Crank` layout are
unchanged, so the template / verification model is identical.

### Instruction Reference (ephemeral)

| Disc | Name               | Accounts                                            | Data             |
| ---: | ------------------ | --------------------------------------------------- | ---------------- |
|    4 | `CreateEphemeral`  | `sponsor(w,s), crank(w), vault(w), magic_program`   | schedule payload |
|    5 | `TriggerEphemeral` | `crank(w), cranker(w,s), instructions_sysvar`       | none             |
|    6 | `CancelEphemeral`  | `authority(w,s), crank(w), vault(w), magic_program` | none             |
|    7 | `CloseEphemeral`   | `reporter(w,s), crank(w), vault(w), magic_program`  | none             |

`vault` is the ephemeral rent vault and `magic_program` is MagicBlock's Magic
program; `hydra-api` exposes both as `consts::magic::EPHEMERAL_VAULT_ID` /
`MAGIC_PROGRAM_ID`, and the `client`-feature builder fills them in. `sponsor` must
be an account delegated to the ER (it pays the rent and sets `authority_signer`).

Build a `CreateEphemeral` with the same `CreateArgs` as base `create`:

```rust
use hydra_api::instruction::{self as ix, CreateArgs, ScheduledIx};

let (crank, _bump) = ix::find_crank_pda(&seed);
let create = ix::create_ephemeral(
    sponsor_pubkey,
    crank,
    &CreateArgs { seed, authority, start_slot: 0, interval_slots: 50,
                  remaining: 0, priority_tip: 0, cu_limit: 0, scheduled },
);
```

### Build & test

#### Live end-to-end test (`tests/e2e`)

`tests/e2e` instead boots the **real** three-process stack — `mb-test-validator` (base L1),
`ephemeral-validator` (the rollup), and `hydra-cranker` — creates a few ephemeral cranks, and
asserts the cranker fires each one on schedule.

The validators ship as an npm package; `mb-test-validator` wraps
`solana-test-validator`, so the Solana/Anza toolchain must also be installed:

```sh
npm install -g @magicblock-labs/ephemeral-validator   # mb-test-validator + ephemeral-validator

# Build the on-chain artifacts the rollup clones (the hydra-cranker is built
# automatically by the test and run with `--ephemeral`).
cargo build-sbf -- --features ephemeral
cargo build-sbf --manifest-path tests/programs/noop/Cargo.toml

# The test is `#[ignore]` (it spawns external validators); run it explicitly.
cargo test --manifest-path tests/e2e/Cargo.toml -- --ignored --nocapture
```

CI runs this as the `e2e` job in `.github/workflows/ci.yml`.

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
