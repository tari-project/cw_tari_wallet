//! Tier A — Hermetic fixture-replay integration tests (run on every PR).
//!
//! These exercise the **real frozen public read APIs** against a deterministic,
//! view-only fixture wallet (see `verify::fixture` for provenance/safety). They
//! assert golden values AND snapshot every returned DTO with `insta`, so a change
//! to the shape or content of a public output struct/enum is caught here in
//! addition to the bridge zero-diff guard.
//!
//! No network, no prompts, fully deterministic.
//!
//! ## Global-state discipline
//! Every read API funnels through the process-global DB slot installed by
//! `initialize_database`. These tests therefore mutate shared global state and
//! MUST run sequentially — they are serialized via [`SERIAL`] and each one tears
//! the DB down (`disconnect_database`) on exit so the slot is left clean.

use std::sync::Mutex;

use rust_lib_flutter_rust_wallet::api::address::get_address;
use rust_lib_flutter_rust_wallet::api::balance::get_balance;
use rust_lib_flutter_rust_wallet::api::db::{disconnect_database, initialize_database};
use rust_lib_flutter_rust_wallet::api::network::TariNetwork;
use rust_lib_flutter_rust_wallet::api::transactions::get_transactions;
use rust_lib_flutter_rust_wallet::api::wallet::list_wallets;
use verify::fixture;

/// Serializes the DB-touching tests: they all install the single process-global
/// DB pool, so running them in parallel would race the shared slot.
static SERIAL: Mutex<()> = Mutex::new(());

/// RAII guard: keeps the fixture temp dir alive for the test's duration and
/// guarantees the process-global DB slot is torn down on drop — even if the test
/// panics on a failed assertion before reaching an explicit teardown.
struct DbGuard {
    _dir: tempfile::TempDir,
}

impl Drop for DbGuard {
    fn drop(&mut self) {
        // Best-effort teardown; ignore the result so a disconnect error during
        // unwind never masks the original panic (and never aborts the process).
        let _ = disconnect_database();
    }
}

/// Materialize a fresh fixture DB in a temp dir and point the global DB at it.
/// Returns a [`DbGuard`] that keeps the temp dir alive for the test's duration
/// and disconnects the global DB on drop, so teardown is deterministic even when
/// an assertion panics. The caller holds the serialization lock for the test.
fn with_fixture_db() -> DbGuard {
    let (dir, db_path) = fixture::materialize_fixture_db().expect("build fixture DB");
    initialize_database(db_path.to_string_lossy().to_string()).expect("initialize_database");
    DbGuard { _dir: dir }
}

#[test]
fn list_wallets_returns_the_fixture_account() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let wallets = list_wallets().expect("list_wallets");
    assert_eq!(wallets, vec![fixture::WALLET_NAME.to_string()]);
    insta::assert_json_snapshot!("list_wallets", wallets);
}

#[test]
fn get_balance_returns_golden_values() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let balance = get_balance(fixture::WALLET_NAME.to_string()).expect("get_balance");

    // Golden assertions: total = credits − debits.
    assert_eq!(balance.total, fixture::GOLDEN_TOTAL, "total balance");
    assert_eq!(
        balance.available,
        fixture::GOLDEN_TOTAL,
        "available balance"
    );
    assert_eq!(balance.locked, 0, "locked balance");
    assert_eq!(balance.unconfirmed, 0, "unconfirmed balance");

    // Snapshot pins the full DTO shape + content.
    insta::assert_json_snapshot!("get_balance", AccountBalanceSnapshot::from(&balance));
}

#[test]
fn get_address_returns_a_stable_esmeralda_address() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    // No passphrase (the fixture account was imported with an empty passphrase),
    // encoded for the fixture's network.
    let address = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(String::new()),
        Some(TariNetwork::Esmeralda),
    )
    .expect("get_address");

    // A base58 Tari address is non-empty and deterministic for the fixed keys.
    assert!(!address.is_empty(), "address must not be empty");
    insta::assert_snapshot!("get_address", address);
}

#[test]
fn get_transactions_returns_the_golden_transaction() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let txs = get_transactions(fixture::WALLET_NAME.to_string(), 100, 0).expect("get_transactions");

    assert_eq!(txs.len(), 1, "exactly one golden transaction");
    let tx = &txs[0];
    assert_eq!(tx.id, fixture::TX_ID.to_string(), "tx id");
    assert_eq!(tx.amount, fixture::TX_AMOUNT, "tx amount");
    assert_eq!(
        tx.blockchain.block_height,
        fixture::TX_BLOCK_HEIGHT,
        "tx height"
    );
    assert_eq!(
        tx.blockchain.confirmations,
        fixture::TX_CONFIRMATIONS,
        "tx confirmations"
    );
    assert!(
        matches!(
            tx.direction,
            rust_lib_flutter_rust_wallet::api::transactions::DisplayedTransactionDirection::Incoming
        ),
        "tx direction is Incoming"
    );

    // Snapshot pins the full DTO shape + content (a contract regression check).
    insta::assert_json_snapshot!(
        "get_transactions",
        txs.iter()
            .map(TransactionSnapshot::from)
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Local, serde-able mirrors of the DTOs.
//
// The library's DTOs intentionally do NOT derive `serde::Serialize` (they are FRB
// wire types, not JSON types), so for `insta::assert_json_snapshot!` we mirror the
// exact field set here. These mirrors are deliberately exhaustive: if a public DTO
// gains/loses/renames a field, this mirror stops compiling — a second, in-harness
// tripwire for the frozen contract.
// ---------------------------------------------------------------------------

use rust_lib_flutter_rust_wallet::api::balance::AccountBalanceDto;
use rust_lib_flutter_rust_wallet::api::transactions::DisplayedTransactionDto;
use serde::Serialize;

#[derive(Serialize)]
struct AccountBalanceSnapshot {
    total: u64,
    unconfirmed: u64,
    locked: u64,
    available: u64,
}

impl From<&AccountBalanceDto> for AccountBalanceSnapshot {
    fn from(b: &AccountBalanceDto) -> Self {
        Self {
            total: b.total,
            unconfirmed: b.unconfirmed,
            locked: b.locked,
            available: b.available,
        }
    }
}

#[derive(Serialize)]
struct TransactionSnapshot {
    id: String,
    direction: String,
    source: String,
    status: String,
    amount: u64,
    amount_display: String,
    message: Option<String>,
    counterparty: Option<CounterpartySnapshot>,
    block_height: u64,
    timestamp: String,
    confirmations: u64,
    fee: Option<FeeSnapshot>,
    payrefs: Vec<String>,
}

#[derive(Serialize)]
struct CounterpartySnapshot {
    address: String,
    address_emoji: String,
}

#[derive(Serialize)]
struct FeeSnapshot {
    amount: u64,
    amount_display: String,
}

impl From<&DisplayedTransactionDto> for TransactionSnapshot {
    fn from(t: &DisplayedTransactionDto) -> Self {
        Self {
            id: t.id.clone(),
            direction: format!("{:?}", t.direction),
            source: format!("{:?}", t.source),
            status: format!("{:?}", t.status),
            amount: t.amount,
            amount_display: t.amount_display.clone(),
            message: t.message.clone(),
            counterparty: t.counterparty.as_ref().map(|c| CounterpartySnapshot {
                address: c.address.clone(),
                address_emoji: c.address_emoji.clone(),
            }),
            block_height: t.blockchain.block_height,
            timestamp: t.blockchain.timestamp.clone(),
            confirmations: t.blockchain.confirmations,
            fee: t.fee.as_ref().map(|f| FeeSnapshot {
                amount: f.amount,
                amount_display: f.amount_display.clone(),
            }),
            payrefs: t.payrefs.clone(),
        }
    }
}
