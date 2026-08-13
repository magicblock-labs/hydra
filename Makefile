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

SHELL := /bin/bash
.DEFAULT_GOAL := help

# Manifests for the crates that live outside the default workspace build.
BASE_MANIFEST     := programs/hydra/Cargo.toml
NOOP_MANIFEST      := tests/programs/noop/Cargo.toml
NATIVE_MANIFEST    := examples/native/Cargo.toml
PINOCCHIO_MANIFEST := examples/pinocchio/Cargo.toml
ANCHOR_MANIFEST    := examples/anchor/Cargo.toml

CLIPPY := --all-targets -- -D warnings

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# ---------------------------------------------------------------------------
# Build — on-chain SBF programs (artifacts land in target/deploy/*.so).
# ---------------------------------------------------------------------------
.PHONY: build build-examples build-anchor

build: ## build-sbf the noop + base + ephemeral hydra programs
	cargo build-sbf --manifest-path $(NOOP_MANIFEST)
	cargo build-sbf --manifest-path $(BASE_MANIFEST)

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
.PHONY: fmt fmt-check lint

fmt: ## Format the workspace and the excluded anchor example
	cargo fmt --all
	cargo fmt --manifest-path $(ANCHOR_MANIFEST) --all

fmt-check: ## Check formatting without writing (CI)
	cargo fmt --all --check
	cargo fmt --manifest-path $(ANCHOR_MANIFEST) --all --check

lint: ## Clippy the workspace and check the excluded anchor example
	cargo clippy --workspace $(CLIPPY)
	cargo check --manifest-path $(ANCHOR_MANIFEST) --all-targets

# ---------------------------------------------------------------------------
# Test. `hydra-tests` and the example mollusk tests load the compiled .so
# files at runtime, so the build targets are prerequisites.
# ---------------------------------------------------------------------------
.PHONY: test test-examples test-anchor test-all bench cu-table

test: build ## Run the hydra-tests suite (unit + integration, via nextest)
	cargo nextest run -p hydra-tests

test-examples: build build-examples ## Run the native + pinocchio example mollusk tests
	cargo nextest run -p hydra-example-native -p hydra-example-pinocchio

test-anchor: build build-anchor ## Run the anchor example mollusk test (needs the anchor CLI)
	cd examples/anchor && anchor run test

test-all: test test-examples test-anchor ## Run hydra-tests and every example mollusk test

bench: build ## Run the compute-unit benchmarks
	cargo bench -p hydra-tests

cu-table: build ## Print the per-instruction CU table (the ignored cu_table test)
	cargo test -p hydra-tests cu_table -- --ignored --nocapture

# ---------------------------------------------------------------------------
# Aggregate / housekeeping.
# ---------------------------------------------------------------------------
.PHONY: ci install-tools clean

ci: fmt-check lint build test-all ## Run the default CI job locally (fmt-check + lint + build + test-all)

install-tools: ## Install cargo-nextest (Solana/anchor/node toolchains are installed separately)
	cargo install cargo-nextest --locked

clean: ## Remove Cargo build artifacts
	cargo clean
