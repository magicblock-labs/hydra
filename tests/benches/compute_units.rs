//! Hydra per-instruction CU bench.
//!
//! Thin wrapper over [`hydra_tests::print_cu_table`] so that both
//! `cargo bench -p hydra-tests` and
//! `cargo test -p hydra-tests cu_table -- --ignored --nocapture` produce
//! the same printed table. Prereqs:
//!
//! ```sh
//! cargo build-sbf --manifest-path programs/hydra/Cargo.toml
//! cargo build-sbf --manifest-path tests/programs/noop/Cargo.toml
//! ```

fn main() {
    // Silence the default Solana runtime logger — we parse CU numbers via
    // mollusk's LogCollector in `print_cu_table`, not stdout.
    solana_logger::setup_with("");
    hydra_tests::print_cu_table();
}
