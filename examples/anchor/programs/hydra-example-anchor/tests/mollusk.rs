//! Mollusk-SVM test for the Anchor example.
//!
//! Follows the pattern from <https://www.anchor-lang.com/docs/testing/mollusk>:
//! build the instruction manually, process it through a `Mollusk` instance,
//! assert on `TransactionResult`.
//!
//! Uses Anchor's generated `InstructionData` + `ToAccountMetas` bindings so
//! discriminator hashing and account-list construction stay auto-in-sync
//! with the `#[program]` and `#[derive(Accounts)]` definitions.
//!
//! Prerequisites (both `.so`s must exist):
//! ```sh
//! cd ../../../../ && cargo build-sbf --manifest-path programs/hydra/Cargo.toml
//! cd examples/anchor && anchor build
//! ```

use anchor_lang::{InstructionData, ToAccountMetas};
use mollusk_svm::{program::keyed_account_for_system_program, Mollusk};
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

use hydra_example_anchor::{accounts as schedule_accts, instruction as schedule_ix, ID as EXAMPLE_ID};

/// Path to the Anchor example `.so` (without extension), relative to this
/// crate's `CARGO_MANIFEST_DIR`.
const EXAMPLE_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/deploy/hydra_example_anchor"
);

/// Path to the root-workspace Hydra `.so`, relative to this crate's manifest.
const HYDRA_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../target/deploy/hydra"
);

fn hydra_id() -> Pubkey {
    Pubkey::new_from_array(hydra_api::ID.to_bytes())
}

#[test]
fn schedule_creates_crank_via_cpi_into_hydra() {
    // Skip gracefully if prerequisites aren't built yet.
    if !std::path::Path::new(&format!("{HYDRA_SO}.so")).exists() {
        eprintln!(
            "skipping: {HYDRA_SO}.so not found. \
             Run `cargo build-sbf` on the root Hydra workspace first."
        );
        return;
    }
    if !std::path::Path::new(&format!("{EXAMPLE_SO}.so")).exists() {
        eprintln!(
            "skipping: {EXAMPLE_SO}.so not found. \
             Run `anchor build` in examples/anchor first."
        );
        return;
    }

    // Mollusk harness: example program as primary, Hydra loaded for CPI.
    let mut mollusk = Mollusk::new(&EXAMPLE_ID, EXAMPLE_SO);
    mollusk.add_program(&hydra_id(), HYDRA_SO);

    // Derive the crank PDA the example will create.
    let seed = [0x11u8; 32];
    let (crank_addr, _bump) = hydra_api::state::find_crank_pda(&seed);
    let crank = Pubkey::new_from_array(crank_addr.to_bytes());
    let payer = Pubkey::new_unique();
    let target_program_id = Pubkey::new_unique();

    let (system_program, system_program_acct) = keyed_account_for_system_program();

    // Anchor generates `accounts::Schedule` (for metas) and
    // `instruction::Schedule` (for args + discriminator). Using them
    // means the test automatically tracks any future signature change.
    let mut metas = schedule_accts::Schedule {
        payer,
        crank,
        system_program,
    }
    .to_account_metas(None);
    // Anchor's `Schedule` struct only declares the three accounts the
    // handler directly touches. The CPIed program (Hydra) must still
    // appear in the tx's account set so the runtime can resolve the
    // CPI target.
    metas.push(AccountMeta::new_readonly(hydra_id(), false));

    let ix = Instruction {
        program_id: EXAMPLE_ID,
        accounts: metas,
        data: schedule_ix::Schedule {
            seed,
            target_program_id,
        }
        .data(),
    };

    let hydra_elf = std::fs::read(format!("{HYDRA_SO}.so")).expect("read hydra .so");
    let hydra_program_acct =
        mollusk_svm::program::create_program_account_loader_v2(&hydra_elf);

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

    // Assert the crank was created with the Anchor example's hard-coded
    // parameters (interval=400, remaining=10, tip=1_000, authority=0,
    // scheduled_data=b"tick").
    let crank_acct = result
        .resulting_accounts
        .iter()
        .find(|(k, _)| k == &crank)
        .map(|(_, a)| a)
        .expect("crank in resulting accounts");
    assert_eq!(crank_acct.owner, hydra_id());

    // Header offsets come from `hydra_api::state::Crank`.
    let data = &crank_acct.data;
    let next_exec_slot = u64::from_le_bytes(data[64..72].try_into().unwrap());
    let interval_slots = u64::from_le_bytes(data[72..80].try_into().unwrap());
    let remaining = u64::from_le_bytes(data[80..88].try_into().unwrap());
    let priority_tip = u64::from_le_bytes(data[88..96].try_into().unwrap());
    assert_eq!(next_exec_slot, 0);
    assert_eq!(interval_slots, 400);
    assert_eq!(remaining, 10);
    assert_eq!(priority_tip, 1_000);

    // Tail region: [num_accounts u16][program_id 32][data_len u16][data].
    let tail = &data[hydra_api::consts::CRANK_HEADER_SIZE..];
    assert_eq!(&tail[2..2 + 32], &target_program_id.to_bytes());
    let data_len = u16::from_le_bytes([tail[2 + 32], tail[2 + 32 + 1]]) as usize;
    assert_eq!(data_len, 4);
    assert_eq!(&tail[2 + 32 + 2..2 + 32 + 2 + 4], b"tick");
}
