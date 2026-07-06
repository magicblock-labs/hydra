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
#   - anchor CLI            -> only for `make build-examples`
#   - @magicblock-labs/ephemeral-validator (npm) -> only for `make test-e2e`

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

CLIPPY := --all-targets -- -D warnings

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------------------
# Build — on-chain SBF programs (artifacts land in target/deploy/*.so).
# ---------------------------------------------------------------------------
.PHONY: build build-base build-ephemeral build-noop build-examples

build: build-base build-ephemeral ## build-sbf the base + ephemeral hydra programs

build-base: build-noop ## build-sbf the base hydra program
	cargo build-sbf --manifest-path $(BASE_MANIFEST)

build-ephemeral: build-noop ## build-sbf the ephemeral-rollup hydra program
	cargo build-sbf --manifest-path $(EPHEMERAL_MANIFEST)

build-noop: ## build-sbf the noop test program (target of scheduled ixs)
	cargo build-sbf --manifest-path $(NOOP_MANIFEST)

build-examples: ## build-sbf the native + pinocchio example programs
	cargo build-sbf --manifest-path $(NATIVE_MANIFEST)
	cargo build-sbf --manifest-path $(PINOCCHIO_MANIFEST)
	cd examples/anchor && anchor build

# ---------------------------------------------------------------------------
# Format & lint (mirrors the fmt + default CI jobs).
# ---------------------------------------------------------------------------
.PHONY: fmt fmt-check lint lint-e2e lint-ephemeral

fmt: ## Format the workspace
	cargo fmt --all

fmt-check: ## Check formatting without writing (CI)
	cargo fmt --all --check

lint: ## Clippy the workspace and check the excluded anchor example
	cargo clippy --workspace $(CLIPPY)
	cargo check --manifest-path $(ANCHOR_MANIFEST) --all-targets

lint-e2e: ## Clippy only the e2e crate
	cargo clippy --manifest-path $(E2E_MANIFEST) $(CLIPPY)

# ---------------------------------------------------------------------------
# Test. `hydra-tests` and the example mollusk tests load the compiled .so
# files at runtime, so the build targets are prerequisites.
# ---------------------------------------------------------------------------
.PHONY: test test-examples test-e2e test-all bench cu-table

test: build-base ## Run the hydra-tests suite (unit + integration, via nextest)
	cargo nextest run -p hydra-tests

test-examples: build-base build-examples ## Run the native + pinocchio example mollusk tests
	cargo nextest run -p hydra-example-native -p hydra-example-pinocchio

test-e2e: build-ephemeral build-noop ## Live e2e: spawns validators + cranker (needs the ephemeral-validator npm pkg)
	cargo test --manifest-path $(E2E_MANIFEST) -- --ignored --nocapture --test-threads=1

test-all: test test-examples test-e2e ## Run hydra-tests, examples, and live e2e

bench: build-base ## Run the compute-unit benchmarks
	cargo bench -p hydra-tests

cu-table: build-base ## Print the per-instruction CU table (the ignored cu_table test)
	cargo test -p hydra-tests cu_table -- --ignored --nocapture

# ---------------------------------------------------------------------------
# Aggregate / housekeeping.
# ---------------------------------------------------------------------------
.PHONY: ci install-tools clean

ci: fmt-check lint build test ## Run the default CI job locally (fmt-check + lint + build + test)

install-tools: ## Install cargo-nextest (Solana/anchor/node toolchains are installed separately)
	cargo install cargo-nextest --locked

clean: ## Remove Cargo build artifacts
	cargo clean
