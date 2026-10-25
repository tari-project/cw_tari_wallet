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
use rust_lib_flutter_rust_wallet::api::send_transaction::{
    send_transaction_with_handler, SendTransactionDetails,
};
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

/// A send whose seed words do not belong to the account is rejected **before** any
/// UTXO is reserved, and leaves the balance completely untouched.
///
/// Since the `minotari af78477` / `tari_* 5.7.0-pre.8` bump, `sign_locked_transaction`
/// verifies a payload-integrity signature, so a seed/account mismatch fails at the
/// *signing* step — which happens after `start_new_transaction` has already flipped the
/// selected outputs to `Locked`. Nothing in this embedding unlocks them again (the
/// expiry sweep only runs in minotari's daemon, which Cake Wallet does not link), so the
/// pre-flight check this asserts is what keeps a mistyped seed word from stranding funds.
///
/// **What this does and does not prove.** It proves the mismatch is caught on the
/// pre-lock path, carries the non-accusatory message, and moves no balance. It cannot
/// demonstrate the stranding scenario itself: the Tier A fixture is a **view-only**
/// account with no spendable outputs, so there is nothing here for
/// `start_new_transaction` to lock even without the guard. Exercising the
/// lock-then-fail-then-release path needs a funded wallet and therefore belongs to
/// Tier C.
#[test]
fn send_with_foreign_seed_words_is_rejected_without_touching_the_balance() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let before = get_balance(fixture::WALLET_NAME.to_string()).expect("balance before");

    // A valid mnemonic for some *other* wallet, generated in-test so no real seed is
    // ever committed. It is overwhelmingly unlikely to share the fixture's view key.
    let foreign_seed_words = fixture::random_seed_words();

    // Send to the fixture's own address, so the only thing wrong is the seed words.
    let recipient = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(String::new()),
        Some(TariNetwork::Esmeralda),
    )
    .expect("fixture address");

    let details = SendTransactionDetails {
        seed_words: foreign_seed_words,
        passphrase: Some(String::new()),
        network: Some(TariNetwork::Esmeralda),
        base_url: None,
        wallet_name: fixture::WALLET_NAME.to_string(),
        recipient_address: recipient,
        amount: 1_000,
        payment_id: None,
        confirmation_window: None,
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(send_transaction_with_handler(details, |_| {}));

    let err = result.expect_err("a foreign seed must not be accepted");
    let rendered = err.to_string();
    assert!(
        rendered.contains("do not match this wallet account"),
        "must name the real cause, got {rendered:?}"
    );
    assert!(
        !rendered.to_lowercase().contains("tampered"),
        "must not accuse the user of tampering, got {rendered:?}"
    );

    // Nothing was reserved: no funds moved into `locked`, and the spendable balance is
    // exactly what it was before the attempt.
    let after = get_balance(fixture::WALLET_NAME.to_string()).expect("balance after");
    assert_eq!(
        after.locked, 0,
        "no UTXO may be left locked by a failed send"
    );
    assert_eq!(
        after.available, before.available,
        "available must not change"
    );
    assert_eq!(after.total, before.total, "total must not change");
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

#[test]
fn confirmation_refresh_persists_scanned_depth_without_confirming_unmined_sends() {
    use minotari_wallet::db::{
        get_accounts, get_displayed_transactions_by_account, insert_displayed_transaction,
        insert_scanned_tip_block, update_displayed_transaction_confirmations,
    };
    use minotari_wallet::transactions::{TransactionDirection, TransactionDisplayStatus as Status};
    use rust_lib_flutter_rust_wallet::api::transactions::{
        refresh_transaction_confirmations, DisplayedTransactionStatus,
    };
    use tari_common_types::types::FixedHash;

    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();
    let conn = rusqlite::Connection::open(db._dir.path().join("wallet.db")).unwrap();
    let account = get_accounts(&conn, Some(fixture::WALLET_NAME))
        .unwrap()
        .remove(0);
    let mut receipt = get_displayed_transactions_by_account(&conn, account.id)
        .unwrap()
        .remove(0);
    receipt.status = Status::Unconfirmed;
    receipt.blockchain.confirmations = 0;
    receipt.blockchain.block_hash = FixedHash::from([1u8; 32]);
    update_displayed_transaction_confirmations(&conn, &receipt).unwrap();
    // A second synthetic account in this fixture must not be updated by the
    // first wallet's scan, even though both histories share one SQLite DB.
    conn.execute(
        "INSERT INTO accounts (friendly_name, fingerprint, encrypted_wallet, cipher_nonce, salt, birthday)
         SELECT 'other-fixture', ?1, encrypted_wallet, cipher_nonce, salt, birthday
         FROM accounts WHERE id = ?2",
        rusqlite::params![vec![2u8; 32], account.id],
    ).unwrap();
    let other_account_id = conn.last_insert_rowid();
    let mut other_receipt = receipt.clone();
    other_receipt.id = 44u64.into();
    other_receipt.details.account_id = other_account_id;
    insert_displayed_transaction(&conn, &other_receipt).unwrap();
    insert_scanned_tip_block(
        &conn,
        other_account_id,
        (fixture::TX_BLOCK_HEIGHT + 100) as i64,
        &[4u8; 32],
    )
    .unwrap();
    let mut pending_send = receipt.clone();
    pending_send.id = 43u64.into();
    pending_send.direction = TransactionDirection::Outgoing;
    pending_send.status = Status::Pending;
    pending_send.blockchain.block_height = 0;
    insert_displayed_transaction(&conn, &pending_send).unwrap();

    // No scanned tip means no evidence to confirm anything, even a mined receipt.
    assert!(
        refresh_transaction_confirmations(fixture::WALLET_NAME.into(), None)
            .unwrap()
            .is_empty()
    );
    insert_scanned_tip_block(
        &conn,
        account.id,
        (fixture::TX_BLOCK_HEIGHT + 2) as i64,
        &[2u8; 32],
    )
    .unwrap();
    let updated = refresh_transaction_confirmations(fixture::WALLET_NAME.into(), None).unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].id, fixture::TX_ID.to_string());
    assert_eq!(updated[0].blockchain.confirmations, 2);
    assert!(matches!(
        updated[0].status,
        DisplayedTransactionStatus::Unconfirmed
    ));
    assert!(
        refresh_transaction_confirmations(fixture::WALLET_NAME.into(), None)
            .unwrap()
            .is_empty()
    );

    insert_scanned_tip_block(
        &conn,
        account.id,
        (fixture::TX_BLOCK_HEIGHT + 3) as i64,
        &[3u8; 32],
    )
    .unwrap();
    let updated = refresh_transaction_confirmations(fixture::WALLET_NAME.into(), None).unwrap();
    assert_eq!(updated.len(), 1);
    assert!(matches!(
        updated[0].status,
        DisplayedTransactionStatus::Confirmed
    ));
    let persisted = get_transactions(fixture::WALLET_NAME.into(), 100, 0).unwrap();
    let receipt = persisted
        .iter()
        .find(|tx| tx.id == fixture::TX_ID.to_string())
        .unwrap();
    assert_eq!(receipt.blockchain.confirmations, 3);
    assert!(matches!(
        receipt.status,
        DisplayedTransactionStatus::Confirmed
    ));
    let pending_send = persisted.iter().find(|tx| tx.id == "43").unwrap();
    assert_eq!(pending_send.blockchain.confirmations, 0);
    assert!(matches!(
        pending_send.status,
        DisplayedTransactionStatus::Pending
    ));
    let other = get_transactions("other-fixture".into(), 100, 0).unwrap();
    assert_eq!(other.len(), 1);
    assert_eq!(other[0].blockchain.confirmations, 0);
    assert!(matches!(
        other[0].status,
        DisplayedTransactionStatus::Unconfirmed
    ));
    assert!(refresh_transaction_confirmations("missing-wallet".into(), None).is_err());
    assert!(refresh_transaction_confirmations(fixture::WALLET_NAME.into(), Some(0)).is_err());
    assert!(
        refresh_transaction_confirmations(fixture::WALLET_NAME.into(), None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        get_balance(fixture::WALLET_NAME.into()).unwrap().total,
        fixture::GOLDEN_TOTAL
    );
}

#[test]
fn outgoing_history_repairs_mined_status_payref_and_signed_fee_without_changing_ledger() {
    use minotari_wallet::db::{
        create_completed_transaction, get_accounts, get_displayed_transactions_by_account,
        insert_displayed_transaction, insert_scanned_tip_block,
    };
    use minotari_wallet::transactions::{
        FeeInfo, TransactionDirection, TransactionDisplayStatus as Status,
    };
    use rust_lib_flutter_rust_wallet::api::transactions::{
        refresh_transaction_confirmations, DisplayedTransactionStatus,
    };
    use tari_common_types::{payment_reference::generate_payment_reference, types::FixedHash};
    use tari_transaction_components::{
        transaction_components::{Transaction, TransactionKernel},
        MicroMinotari,
    };
    use tari_utilities::ByteArray;

    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();
    let conn = rusqlite::Connection::open(db._dir.path().join("wallet.db")).unwrap();
    let account = get_accounts(&conn, Some(fixture::WALLET_NAME))
        .unwrap()
        .remove(0);
    let mut send = get_displayed_transactions_by_account(&conn, account.id)
        .unwrap()
        .remove(0);
    // Exercise unsigned u64 tx IDs stored as wrapped i64 in completed history.
    send.id = (u64::MAX - 1).into();
    send.direction = TransactionDirection::Outgoing;
    send.status = Status::Pending;
    send.blockchain.block_height = 0;
    send.blockchain.block_hash = FixedHash::zero();
    send.blockchain.confirmations = 0;
    send.amount = MicroMinotari(10_000_700);
    send.fee = Some(FeeInfo {
        amount: MicroMinotari(0),
    });
    send.details.total_debit = MicroMinotari(100_000_000);
    send.details.total_credit = MicroMinotari(89_999_300);
    let recipient_hash = FixedHash::from([41_u8; 32]);
    send.details.sent_output_hashes = vec![recipient_hash];
    let change_hash = FixedHash::from([45_u8; 32]);
    send.details.outputs = vec![minotari_wallet::transactions::TransactionOutput {
        hash: change_hash,
        amount: MicroMinotari(89_999_300),
        status: minotari_wallet::models::OutputStatus::Unspent,
        mined_in_block_height: 0,
        mined_in_block_hash: FixedHash::zero(),
        output_type: tari_transaction_components::transaction_components::OutputType::Standard,
        is_change: true,
    }];
    let provisional = generate_payment_reference(&FixedHash::zero(), &recipient_hash);
    send.details.sent_payrefs = vec![provisional];
    insert_displayed_transaction(&conn, &send).unwrap();
    conn.execute(
        "INSERT INTO pending_transactions (id, account_id, idempotency_key, status, expires_at)
         VALUES ('outgoing-fixture', ?1, 'outgoing-fixture', 'completed', '2030-01-01 00:00:00')",
        [account.id],
    )
    .unwrap();
    let signed = Transaction::new(
        vec![],
        vec![],
        vec![TransactionKernel::new_current_version(
            Default::default(),
            MicroMinotari(700),
            0,
            Default::default(),
            Default::default(),
            None,
        )],
        Default::default(),
        Default::default(),
    );
    create_completed_transaction(
        &conn,
        account.id,
        "outgoing-fixture",
        &[0_u8; 32],
        &serde_json::to_vec(&signed).unwrap(),
        None,
        send.id,
    )
    .unwrap();

    let read_send = || {
        use rust_lib_flutter_rust_wallet::api::transactions::get_changed_transactions;
        let changes = get_changed_transactions(
            fixture::WALLET_NAME.into(),
            Some(vec![(u64::MAX - 1).to_string()]),
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert!(get_changed_transactions(fixture::WALLET_NAME.into(), None)
            .unwrap()
            .is_empty());
        changes.into_iter().next().unwrap()
    };
    let dto = read_send();
    assert_eq!(dto.amount, 10_000_000);
    assert_eq!(dto.fee.unwrap().amount, 700);
    assert!(matches!(dto.status, DisplayedTransactionStatus::Pending));
    assert!(dto.payrefs.is_empty());

    // Simulate 0.3.0's scanner: it finds the block but leaves the pending hash
    // and PayRef untouched. Use deterministic synthetic hashes, no funded wallet.
    send.status = Status::Unconfirmed;
    send.blockchain.block_height = fixture::TX_BLOCK_HEIGHT;
    conn.execute(
        "DELETE FROM displayed_transactions WHERE id = ?1",
        [send.id.to_string()],
    )
    .unwrap();
    insert_displayed_transaction(&conn, &send).unwrap();
    let block_hash = FixedHash::from([42_u8; 32]);
    insert_scanned_tip_block(
        &conn,
        account.id,
        fixture::TX_BLOCK_HEIGHT as i64,
        block_hash.as_bytes(),
    )
    .unwrap();
    insert_scanned_tip_block(
        &conn,
        account.id,
        (fixture::TX_BLOCK_HEIGHT + 2) as i64,
        &[43_u8; 32],
    )
    .unwrap();
    refresh_transaction_confirmations(fixture::WALLET_NAME.into(), None).unwrap();
    let dto = read_send();
    assert_eq!(dto.blockchain.confirmations, 2);
    assert!(matches!(
        dto.status,
        DisplayedTransactionStatus::Unconfirmed
    ));
    let reference = generate_payment_reference(&block_hash, &recipient_hash).to_string();
    assert_ne!(reference, provisional.to_string());
    assert_eq!(dto.payrefs, vec![reference.clone()]);

    insert_scanned_tip_block(
        &conn,
        account.id,
        (fixture::TX_BLOCK_HEIGHT + 3) as i64,
        &[44_u8; 32],
    )
    .unwrap();
    refresh_transaction_confirmations(fixture::WALLET_NAME.into(), None).unwrap();
    let dto = read_send();
    assert!(matches!(dto.status, DisplayedTransactionStatus::Confirmed));
    assert_eq!(dto.blockchain.confirmations, 3);
    assert_eq!(dto.blockchain.block_height, fixture::TX_BLOCK_HEIGHT);
    assert_eq!(dto.amount, 10_000_000);
    assert_eq!(dto.fee.unwrap().amount, 700);
    assert_eq!(dto.payrefs, vec![reference.clone()]);
    let stored = get_displayed_transactions_by_account(&conn, account.id)
        .unwrap()
        .into_iter()
        .find(|tx| tx.id == send.id)
        .unwrap();
    assert_eq!(stored.amount.0, 10_000_700);
    assert_eq!(stored.details.total_debit.0, 100_000_000);
    assert_eq!(stored.details.total_credit.0, 89_999_300);
    assert_eq!(stored.blockchain.block_hash, block_hash);
    assert_eq!(
        get_balance(fixture::WALLET_NAME.into()).unwrap().total,
        fixture::GOLDEN_TOTAL
    );
    // Incoming history and its golden amounts are unaffected.
    let receipt = get_transactions(fixture::WALLET_NAME.into(), 100, 0)
        .unwrap()
        .into_iter()
        .find(|tx| tx.id == fixture::TX_ID.to_string())
        .unwrap();
    assert_eq!(receipt.amount, fixture::TX_AMOUNT);

    // Read repair also covers old Confirmed rows and pruned scanned blocks.
    // With no actual block hash available, suppress the provisional reference.
    let mut old_send = stored;
    old_send.blockchain.block_hash = FixedHash::zero();
    minotari_wallet::db::update_displayed_transaction_confirmations(&conn, &old_send).unwrap();
    conn.execute(
        "DELETE FROM scanned_tip_blocks WHERE account_id = ?1 AND height = ?2",
        rusqlite::params![account.id, fixture::TX_BLOCK_HEIGHT as i64],
    )
    .unwrap();
    assert!(read_send().payrefs.is_empty());
    conn.execute(
        "INSERT INTO outputs (account_id, tx_id, output_hash, mined_in_block_hash,
         mined_in_block_height, value, wallet_output_json, mined_timestamp)
         VALUES (?1, 45, ?2, ?3, ?4, 89999300, '{}', '2023-11-14 22:13:20')",
        rusqlite::params![
            account.id,
            change_hash.as_bytes(),
            block_hash.as_bytes(),
            fixture::TX_BLOCK_HEIGHT as i64
        ],
    )
    .unwrap();
    assert_eq!(read_send().payrefs, vec![reference]);
}

#[test]
fn changed_history_retries_bad_rows_and_does_not_read_unrelated_history() {
    use rust_lib_flutter_rust_wallet::api::transactions::get_changed_transactions;
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();
    let conn = rusqlite::Connection::open(db._dir.path().join("wallet.db")).unwrap();
    assert!(get_changed_transactions(fixture::WALLET_NAME.into(), None)
        .unwrap()
        .is_empty());
    let original: String = conn
        .query_row(
            "SELECT transaction_json FROM displayed_transactions WHERE id = ?1",
            [fixture::TX_ID.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE displayed_transactions SET transaction_json = 'invalid-json'",
        [],
    )
    .unwrap();
    // An unchanged cycle must not deserialize even an unrelated malformed row.
    assert!(get_changed_transactions(fixture::WALLET_NAME.into(), None)
        .unwrap()
        .is_empty());
    assert!(get_changed_transactions(
        fixture::WALLET_NAME.into(),
        Some(vec![fixture::TX_ID.to_string()]),
    )
    .is_err());
    conn.execute(
        "UPDATE displayed_transactions SET transaction_json = ?1",
        [&original],
    )
    .unwrap();
    // Failed reads were not acknowledged; a retry needs no new hint/event.
    let changes = get_changed_transactions(fixture::WALLET_NAME.into(), None).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].id, fixture::TX_ID.to_string());
    assert_eq!(changes[0].amount, fixture::TX_AMOUNT);
    assert!(get_changed_transactions(fixture::WALLET_NAME.into(), None)
        .unwrap()
        .is_empty());
}
