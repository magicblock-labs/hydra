//! Mollusk test for the Pinocchio example.
//!
//! Pre-req: both `.so`s must exist.
//! ```sh
//! cargo build-sbf --manifest-path programs/hydra/Cargo.toml
//! cargo build-sbf --manifest-path examples/pinocchio/Cargo.toml
//! ```
//! Then: `cargo test -p hydra-example-pinocchio --test mollusk`.

use mollusk_svm::{program::keyed_account_for_system_program, Mollusk};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{pubkey, Pubkey};

/// Program ID — auto-generated at `target/deploy/hydra_example_pinocchio-keypair.json`
/// by `cargo build-sbf`. Stable across rebuilds unless you delete the keypair.
/// Verify with: `solana address -k target/deploy/hydra_example_pinocchio-keypair.json`
const EXAMPLE_ID: Pubkey = pubkey!("GAMbTP6XFD1PQ88D8v6F33cHWJjc8sSwRhHHzMRNFoKR");
const EXAMPLE_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/deploy/hydra_example_pinocchio"
);
const HYDRA_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/hydra");

/// Discriminator for the example's `schedule` ix. Matches
/// `DISC_SCHEDULE = 0` in `src/lib.rs`.
const DISC_SCHEDULE: u8 = 0;

fn hydra_id() -> Pubkey {
    Pubkey::new_from_array(hydra_api::ID.to_bytes())
}

#[test]
fn schedule_creates_crank_via_cpi_into_hydra() {
    if !std::path::Path::new(&format!("{HYDRA_SO}.so")).exists() {
        eprintln!(
            "skipping: {HYDRA_SO}.so not found. Run `cargo build-sbf` on the Hydra workspace."
        );
        return;
    }
    if !std::path::Path::new(&format!("{EXAMPLE_SO}.so")).exists() {
        eprintln!("skipping: {EXAMPLE_SO}.so not found. Run `cargo build-sbf` on this crate.");
        return;
    }

    let mut mollusk = Mollusk::new(&EXAMPLE_ID, EXAMPLE_SO);
    mollusk.add_program(&hydra_id(), HYDRA_SO);

    // Derive the crank PDA from the seed we'll pass in.
    let seed = [0x55u8; 32];
    let (crank_addr, _bump) = hydra_api::state::find_crank_pda(&seed);
    let crank = Pubkey::new_from_array(crank_addr.to_bytes());
    let payer = Pubkey::new_unique();
    let target_program_id = Pubkey::new_unique();
    let tick: &[u8] = b"tick";

    // Example's schedule data layout:
    //   [disc: 1][seed: 32][target_program_id: 32][tick_len: u16 LE][tick: bytes]
    let mut data = Vec::with_capacity(1 + 32 + 32 + 2 + tick.len());
    data.push(DISC_SCHEDULE);
    data.extend_from_slice(&seed);
    data.extend_from_slice(&target_program_id.to_bytes());
    data.extend_from_slice(&(tick.len() as u16).to_le_bytes());
    data.extend_from_slice(tick);

    let (system_program, system_program_acct) = keyed_account_for_system_program();

    // The pinocchio example explicitly destructures
    // `[payer, crank, system_program, hydra_program, ..]` — hydra must be
    // at index 3.
    let ix = Instruction {
        program_id: EXAMPLE_ID,
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new(crank, false),
            AccountMeta::new_readonly(system_program, false),
            AccountMeta::new_readonly(hydra_id(), false),
        ],
        data,
    };

    let hydra_elf = std::fs::read(format!("{HYDRA_SO}.so")).expect("read hydra .so");
    let hydra_program_acct = mollusk_svm::program::create_program_account_loader_v2(&hydra_elf);

    let accounts = vec![
        (payer, Account::new(1_000_000_000, 0, &system_program)),
        (crank, Account::default()),
        (system_program, system_program_acct),
        (hydra_id(), hydra_program_acct),
    ];

    let result = mollusk.process_transaction_instructions(&[ix], &accounts);
    assert!(
        result.raw_result.is_ok(),
        "schedule failed: {:?}",
        result.raw_result
    );

    let crank_acct = result
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &crank)
        .map(|(_, a)| a)
        .expect("crank in resulting accounts");
    assert_eq!(crank_acct.owner, hydra_id());

    let d = &crank_acct.data;
    let interval = u64::from_le_bytes(d[72..80].try_into().unwrap());
    let remaining = u64::from_le_bytes(d[80..88].try_into().unwrap());
    let tip = u64::from_le_bytes(d[88..96].try_into().unwrap());
    assert_eq!(interval, 400);
    assert_eq!(remaining, 10);
    assert_eq!(tip, 1_000);
}
