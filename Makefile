# Hydra — build & test commands.
#
# Quickstart:
#   make build   # compile the on-chain programs (needed before tests)
#   make test    # run the hydra-tests suite
#   make ci      # everything the default CI job runs, locally
#
# Run `make` or `make help` for the full list.
#
# Toolchain prerequisites:
#   - Rust + `cargo build-sbf` (Solana/Anza toolchain)
#   - cargo-nextest         -> `make install-tools`
#   - anchor CLI            -> only for `make build-anchor` / `make test-anchor`
#   - node + @magicblock-labs/ephemeral-validator -> `make install-validators`
#     (needed by `make test-e2e`, and so by `make test-all` / `make ci`)

SHELL := /bin/bash
.DEFAULT_GOAL := help

# Manifests for the crates that live outside the default workspace build.
BASE_MANIFEST     := programs/hydra/Cargo.toml
EPHEMERAL_MANIFEST := programs/hydra-ephemeral/Cargo.toml
NOOP_MANIFEST      := tests/programs/noop/Cargo.toml
NATIVE_MANIFEST    := examples/native/Cargo.toml
PINOCCHIO_MANIFEST := examples/pinocchio/Cargo.toml
ANCHOR_MANIFEST    := examples/anchor/Cargo.toml
E2E_MANIFEST       := tests/e2e/Cargo.toml

HYDRA_FEATURES := logging,cu-trace
EPHEMERAL_FEATURES := logging

EPHEMERAL_VALIDATOR_VERSION ?= 0.13.20

CLIPPY := --all-targets -- -D warnings

RUSTFLAGS ?= -D warnings
export RUSTFLAGS

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------------------
# Build — on-chain SBF programs (artifacts land in target/deploy/*.so).
# ---------------------------------------------------------------------------
.PHONY: build build-cranker build-examples build-anchor

build: ## build-sbf the noop + hydra programs
	cargo build-sbf --manifest-path $(NOOP_MANIFEST)
	cargo build-sbf --manifest-path $(BASE_MANIFEST)
	cargo build-sbf --manifest-path $(EPHEMERAL_MANIFEST)

# `[profile.release] lto = "thin"` is for the SBF programs. On a host build it
# makes rustc embed LLVM bitcode into the rlibs, which Apple's linker (an older
# libLTO than rustc's LLVM) can't parse — the macOS link then fails. LTO buys
# the cranker nothing, so build it without. Mirrors the publish workflow.
build-cranker: ## Build the release hydra-cranker binary (target/release/hydra-cranker)
	CARGO_PROFILE_RELEASE_LTO=false cargo build -p hydra-cranker --release

build-examples: ## build-sbf the native + pinocchio example programs
	cargo build-sbf --manifest-path $(NATIVE_MANIFEST)
	cargo build-sbf --manifest-path $(PINOCCHIO_MANIFEST)

# `--ignore-keys`: anchor >=1.0 refuses to build when target/deploy's keypair
# doesn't match `declare_id!`, and that keypair is generated per-machine. The
# example is mollusk-only (never deployed), so the on-disk keypair is moot.
build-anchor: ## anchor build the anchor example (needs the anchor CLI)
	cd examples/anchor && anchor build --ignore-keys

# ---------------------------------------------------------------------------
# Format & lint (mirrors the fmt + default CI jobs).
# ---------------------------------------------------------------------------
.PHONY: fmt fmt-check lint lint-e2e

fmt: ## Format the workspace and the excluded anchor / e2e crates
	cargo fmt --all
	cargo fmt --manifest-path $(ANCHOR_MANIFEST) --all
	cargo fmt --manifest-path $(E2E_MANIFEST) --all

fmt-check: ## Check formatting without writing (CI)
	cargo fmt --all --check
	cargo fmt --manifest-path $(ANCHOR_MANIFEST) --all --check
	cargo fmt --manifest-path $(E2E_MANIFEST) --all --check

lint: ## Clippy the workspace, both programs' optional features, and the anchor example (not e2e — see lint-e2e)
	cargo clippy --workspace $(CLIPPY)
	cargo clippy -p hydra --features $(HYDRA_FEATURES) $(CLIPPY)
	cargo clippy -p hydra-ephemeral --features $(EPHEMERAL_FEATURES) $(CLIPPY)
	cargo clippy --manifest-path $(ANCHOR_MANIFEST) $(CLIPPY)

lint-e2e: ## Clippy only the e2e crate
	cargo clippy --manifest-path $(E2E_MANIFEST) $(CLIPPY)

# ---------------------------------------------------------------------------
# Test. `hydra-tests` and the example mollusk tests load the compiled .so
# files at runtime, so the build targets are prerequisites.
# ---------------------------------------------------------------------------
.PHONY: test test-examples test-anchor test-e2e test-all bench cu-table

test: build build-examples ## Run every workspace test (via nextest)
	cargo nextest run --workspace

test-examples: build build-examples ## Run just the native + pinocchio example mollusk tests
	cargo nextest run -p hydra-example-native -p hydra-example-pinocchio

test-anchor: build build-anchor ## Run the anchor example mollusk test (needs the anchor CLI)
	cd examples/anchor && anchor run test

test-e2e: build ## Live e2e: spawns validators + cranker (needs the ephemeral-validator npm pkg)
	cargo test --manifest-path $(E2E_MANIFEST) -- --ignored --nocapture --test-threads=1

# `test` already covers the examples (they're workspace members).
test-all: test test-anchor test-e2e ## Run the workspace tests (incl. examples), the anchor example, and the live e2e suite

bench: build ## Run the compute-unit benchmarks
	cargo bench -p hydra-tests

cu-table: build ## Print the per-instruction CU table (the ignored cu_table test)
	cargo test -p hydra-tests cu_table -- --ignored --nocapture

# ---------------------------------------------------------------------------
# Aggregate / housekeeping.
# ---------------------------------------------------------------------------
.PHONY: ci install-tools install-validators clean

ci: fmt-check lint lint-e2e build test-all ## Run the CI job locally (fmt-check + lint + build + test-all, incl. live e2e)

install-tools: install-validators ## Install cargo-nextest + the MagicBlock validators
	cargo install cargo-nextest --locked

install-validators: ## npm install -g mb-test-validator + ephemeral-validator (needs node)
	npm install -g "@magicblock-labs/ephemeral-validator@$(EPHEMERAL_VALIDATOR_VERSION)"

clean: ## Remove Cargo build artifacts
	cargo clean
