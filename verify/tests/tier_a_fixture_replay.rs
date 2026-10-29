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

// Spawning a scan needs its (deep) minotari future proven `Send`; see `src/lib.rs`.
#![recursion_limit = "256"]

use std::sync::Mutex;

use rust_lib_flutter_rust_wallet::api::address::get_address;
use rust_lib_flutter_rust_wallet::api::balance::get_balance;
use rust_lib_flutter_rust_wallet::api::db::{disconnect_database, initialize_database};
use rust_lib_flutter_rust_wallet::api::fee::{
    estimate_send_all_fee, estimate_transaction_fee_for_outputs, FeePriority,
};
use rust_lib_flutter_rust_wallet::api::network::current_network;
use rust_lib_flutter_rust_wallet::api::network::TariNetwork;
use rust_lib_flutter_rust_wallet::api::scanner::{
    rescan_v2_with_handler, rescan_with_handler, start_scan_v2_with_handler, ScanConfiguration,
    ScanEventDto, ScanEventV2Dto, ScanStatusDto,
};
use rust_lib_flutter_rust_wallet::api::send_transaction::{
    abandon_send, send_transaction_multi_with_handler, send_transaction_with_handler,
    MultiSendTransactionDetails, SendRecipient, SendTransactionDetails, TransactionStage,
};
use rust_lib_flutter_rust_wallet::api::signing::{sign_message_with_wallet, verify_message};
use rust_lib_flutter_rust_wallet::api::transactions::get_transactions;
use rust_lib_flutter_rust_wallet::api::wallet::{
    change_passphrase, import_wallet_from_keys, list_wallets, restore_wallet,
};
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

/// A send-all goes through the same pre-lock seed check as a single send.
#[test]
fn send_all_with_foreign_seed_words_is_rejected_without_touching_the_balance() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let recipient = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(String::new()),
        Some(TariNetwork::Esmeralda),
    )
    .expect("fixture address");
    let details = MultiSendTransactionDetails {
        seed_words: fixture::random_seed_words(),
        passphrase: Some(String::new()),
        network: Some(TariNetwork::Esmeralda),
        base_url: None,
        wallet_name: fixture::WALLET_NAME.to_string(),
        recipients: vec![SendRecipient {
            address: recipient,
            amount: 0,
            payment_id: None,
        }],
        send_all: true,
        confirmation_window: None,
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(send_transaction_multi_with_handler(details, |_| {}))
        .expect_err("a foreign seed must not be accepted");
    assert!(
        err.to_string().contains("do not match this wallet account"),
        "got {err}"
    );
    let balance = get_balance(fixture::WALLET_NAME.to_string()).expect("balance");
    assert_eq!(balance.locked, 0);
    assert_eq!(balance.total, fixture::GOLDEN_TOTAL);
}

/// Throwaway spend and view secrets for the key-import tests (small scalars, never a
/// real wallet).
const IMPORT_VIEW_KEY_HEX: &str =
    "0500000000000000000000000000000000000000000000000000000000000000";
const IMPORT_SPEND_KEY_HEX: &str =
    "0900000000000000000000000000000000000000000000000000000000000000";

#[test]
fn import_wallet_from_keys_creates_a_wallet_that_can_sign() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let details = import_wallet_from_keys(
        "imported".to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        IMPORT_SPEND_KEY_HEX.to_string(),
        fixture::BIRTHDAY,
        "pw".to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("import_wallet_from_keys");
    assert!(details.seed_words.is_none());
    assert_eq!(details.wallet_birthday, fixture::BIRTHDAY);
    assert_eq!(current_network(), Some(TariNetwork::Esmeralda));
    assert_eq!(details.view_private_key_hex, IMPORT_VIEW_KEY_HEX);

    let mut wallets = list_wallets().expect("list_wallets");
    wallets.sort();
    assert_eq!(
        wallets,
        vec!["imported".to_string(), fixture::WALLET_NAME.to_string()]
    );

    // The stored keys are spendable: signing works, and the address it returns verifies.
    let signature = sign_message_with_wallet("imported".to_string(), "pw".to_string(), "hi".into())
        .expect("a key-imported wallet can sign");
    assert!(verify_message("hi".into(), signature, details.tari_address).expect("verify"));
}

#[test]
fn import_wallet_from_keys_rejects_equal_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let Err(err) = import_wallet_from_keys(
        "same-keys".to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        fixture::BIRTHDAY,
        "pw".to_string(),
        Some(TariNetwork::Esmeralda),
    ) else {
        panic!("equal view and spend keys must be refused");
    };
    assert_eq!(
        err.to_string(),
        "Wallet Error: Failed to import wallet from keys: The view key and the spend key must differ"
    );
    assert_eq!(
        list_wallets().unwrap(),
        vec![fixture::WALLET_NAME.to_string()]
    );
}

#[test]
fn sign_message_with_wallet_refuses_a_view_only_wallet() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let err = sign_message_with_wallet(fixture::WALLET_NAME.to_string(), String::new(), "m".into())
        .expect_err("a view-only wallet cannot sign");
    assert_eq!(
        err.to_string(),
        "Signing Error: View-only wallets cannot sign messages"
    );

    let err =
        sign_message_with_wallet(fixture::WALLET_NAME.to_string(), "wrong".into(), "m".into())
            .expect_err("a wrong passphrase must be refused");
    assert_eq!(err.to_string(), "Invalid Passphrase");

    let err = sign_message_with_wallet("missing".to_string(), String::new(), "m".into())
        .expect_err("an unknown wallet must be refused");
    assert_eq!(err.to_string(), "No accounts found for this wallet");
}

#[test]
fn change_passphrase_reencrypts_the_wallet() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let address_with = |passphrase: &str| {
        get_address(
            fixture::WALLET_NAME.to_string(),
            Some(passphrase.to_string()),
            Some(TariNetwork::Esmeralda),
        )
    };
    let before = address_with("").expect("old passphrase opens the wallet");
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let err = runtime
        .block_on(change_passphrase(
            fixture::WALLET_NAME.to_string(),
            "wrong".to_string(),
            "new".to_string(),
        ))
        .expect_err("a wrong old passphrase must be refused");
    assert_eq!(err.to_string(), "Invalid Passphrase");
    address_with("").expect("a refused change leaves the old passphrase working");

    // A continuous scan is running; changing the passphrase stops it first.
    runtime.block_on(async {
        let server = mock_chain_node().await;
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ScanConfiguration {
            continuous: true,
            ..rescan_config("", server.uri())
        };
        let scan = tokio::spawn(start_scan_v2_with_handler(config, move |_| {
            let _ = seen_tx.send(());
            Ok(())
        }));
        seen_rx.recv().await.expect("the scan is running");

        change_passphrase(
            fixture::WALLET_NAME.to_string(),
            String::new(),
            "new".to_string(),
        )
        .await
        .expect("change_passphrase");
        // The scan had returned (and dropped its event callback) before the change.
        loop {
            match seen_rx.try_recv() {
                Ok(()) => continue,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    panic!("the scan was still running after change_passphrase")
                }
            }
        }
        scan.await.unwrap().expect("a stopped scan returns Ok");
    });
    assert_eq!(address_with("new").expect("new passphrase"), before);
    assert!(
        address_with("").is_err(),
        "the old passphrase no longer opens it"
    );
}

#[test]
fn send_all_estimate_errors_when_nothing_is_spendable() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let recipient = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(String::new()),
        Some(TariNetwork::Esmeralda),
    )
    .expect("fixture address");

    // The fixture's balance is history rows only; it holds no spendable outputs.
    let err = estimate_send_all_fee(
        fixture::WALLET_NAME.to_string(),
        recipient,
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect_err("no spendable outputs");
    assert!(err.to_string().starts_with("Wallet Error: "), "got {err}");

    let err = estimate_send_all_fee(
        fixture::WALLET_NAME.to_string(),
        "not-an-address".to_string(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect_err("bad address");
    assert!(
        err.to_string().starts_with("Invalid Recipient Address: "),
        "got {err}"
    );
}

#[test]
fn multi_output_fee_estimate_errors_when_funds_are_short() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    // An unreachable node makes the estimator fall back to its default fee rates, so
    // this stays offline; the fixture has no spendable outputs to select.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(estimate_transaction_fee_for_outputs(
            1_000,
            2,
            FeePriority::Medium,
            "http://127.0.0.1:1".to_string(),
            fixture::WALLET_NAME.to_string(),
        ))
        .expect_err("no spendable outputs");
    assert_eq!(err.to_string(), "Failed to estimate fees");
}

fn rescan_config(passphrase: &str, base_url: String) -> ScanConfiguration {
    ScanConfiguration {
        wallet_name: fixture::WALLET_NAME.to_string(),
        passphrase: passphrase.to_string(),
        base_url,
        batch_size: 10,
        continuous: false,
        poll_interval_seconds: 1,
        required_confirmations: 3,
    }
}

#[test]
fn rescan_with_a_wrong_passphrase_changes_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(rescan_with_handler(
            0,
            rescan_config("wrong", "http://127.0.0.1:1".to_string()),
            |_| Ok(()),
        ))
        .expect_err("a wrong passphrase must be refused");
    assert_eq!(err.to_string(), "Invalid Passphrase");
    assert_eq!(
        get_balance(fixture::WALLET_NAME.to_string()).unwrap().total,
        fixture::GOLDEN_TOTAL
    );
}

/// Mount the [`MockChain`] responder on a fresh mock node.
async fn mock_chain_node() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(MockChain)
        .mount(&server)
        .await;
    server
}

fn funded_scan_config(passphrase: &str, base_url: String) -> ScanConfiguration {
    ScanConfiguration {
        wallet_name: FUNDED.to_string(),
        ..rescan_config(passphrase, base_url)
    }
}

/// How many outputs the funded wallet could spend now, or `None` if none (read
/// through the send-all quote, which selects from the wallet's outputs).
fn spendable_outputs(funded: &FundedWallet) -> Option<usize> {
    estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .ok()
    .map(|e| e.input_count)
}

/// The funded wallet's outputs are mined at height 10 and its scanned tip matches the
/// mock chain's tip, so without a rollback the scan would keep them. A rescan from 5
/// rolls them back and the mock chain (no outputs) does not have them, so after the
/// rescan completes the wallet has nothing to spend.
#[test]
fn rescan_rolls_back_then_scans_to_the_tip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    assert_eq!(spendable_outputs(&funded), Some(FUNDED_VALUES.len()));

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(async {
            let server = mock_chain_node().await;
            let sink = events.clone();
            rescan_with_handler(
                5,
                funded_scan_config(FUNDED_PASSPHRASE, server.uri()),
                move |event| {
                    sink.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
        })
        .expect("rescan");

    let events = events.lock().unwrap();
    assert!(
        matches!(
            events.last(),
            Some(ScanEventDto::Status(ScanStatusDto::Completed {
                final_height: MOCK_TIP,
                ..
            }))
        ),
        "got {events:?}"
    );
    assert_eq!(
        spendable_outputs(&funded),
        None,
        "the outputs above the rescan height were rolled back"
    );
}

#[test]
fn rescan_v2_rolls_back_then_scans_to_the_tip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let wrong = runtime.block_on(rescan_v2_with_handler(
        5,
        funded_scan_config("wrong", "http://127.0.0.1:1".to_string()),
        |_| Ok(()),
    ));
    assert_eq!(
        wrong.expect_err("wrong passphrase").to_string(),
        "Invalid Passphrase"
    );
    assert_eq!(
        spendable_outputs(&funded),
        Some(FUNDED_VALUES.len()),
        "a refused rescan rolls nothing back"
    );

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    runtime
        .block_on(async {
            let server = mock_chain_node().await;
            let sink = events.clone();
            rescan_v2_with_handler(
                5,
                funded_scan_config(FUNDED_PASSPHRASE, server.uri()),
                move |event| {
                    sink.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
        })
        .expect("rescan_v2");

    let events = events.lock().unwrap();
    assert!(
        matches!(
            events.last(),
            Some(ScanEventV2Dto::Completed {
                final_height: MOCK_TIP,
                ..
            })
        ),
        "got {events:?}"
    );
    assert_eq!(
        spendable_outputs(&funded),
        None,
        "the outputs above the rescan height were rolled back"
    );
}

// ---------------------------------------------------------------------------
// Happy paths on a funded wallet.
//
// `fixture::add_spendable_outputs` gives a seed-words wallet real, signable outputs
// that exist only in the DB. Fee quotes and sends are built and signed from the DB
// alone; the broadcast goes to a mock node that answers nothing, which minotari
// records as "not accepted yet, will rebroadcast" and still returns `Ok`.
// ---------------------------------------------------------------------------

const FUNDED: &str = "funded";
const FUNDED_PASSPHRASE: &str = "pw";
const FUNDED_VALUES: [u64; 3] = [1_000_000, 2_000_000, 3_000_000];

struct FundedWallet {
    _db: DbGuard,
    db_path: std::path::PathBuf,
    seed_words: Vec<String>,
    recipient: String,
}

/// The fixture DB plus a restored seed-words wallet holding [`FUNDED_VALUES`]. The
/// recipient is the fixture's own (view-only) wallet.
fn with_funded_wallet() -> FundedWallet {
    let (dir, db_path) = fixture::materialize_fixture_db().expect("build fixture DB");
    initialize_database(db_path.to_string_lossy().to_string()).expect("initialize_database");
    let db = DbGuard { _dir: dir };

    let seed_words = fixture::random_seed_words();
    restore_wallet(
        FUNDED.to_string(),
        seed_words.clone(),
        FUNDED_PASSPHRASE.to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("restore_wallet");
    fixture::add_spendable_outputs(&db_path, FUNDED, FUNDED_PASSPHRASE, &FUNDED_VALUES)
        .expect("add outputs");

    let recipient = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(String::new()),
        Some(TariNetwork::Esmeralda),
    )
    .expect("fixture address");
    FundedWallet {
        _db: db,
        db_path,
        seed_words,
        recipient,
    }
}

fn funded_send(
    funded: &FundedWallet,
    recipients: Vec<SendRecipient>,
    send_all: bool,
    base_url: String,
) -> MultiSendTransactionDetails {
    MultiSendTransactionDetails {
        seed_words: funded.seed_words.clone(),
        passphrase: Some(FUNDED_PASSPHRASE.to_string()),
        network: Some(TariNetwork::Esmeralda),
        base_url: Some(base_url),
        wallet_name: FUNDED.to_string(),
        recipients,
        send_all,
        confirmation_window: None,
    }
}

#[test]
fn send_all_estimate_quotes_every_output() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let estimate = estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect("estimate_send_all_fee");
    let total: u64 = FUNDED_VALUES.iter().sum();
    assert_eq!(estimate.input_count, 3);
    assert_eq!((estimate.skipped_dust, estimate.skipped_over_cap), (0, 0));
    assert!(estimate.fee > 0);
    // Only the minimum change is left over besides the fee.
    assert!(estimate.amount_after_fee + estimate.fee < total);
    assert!(estimate.amount_after_fee + estimate.fee > total - 10_000);

    // The confirmation window is honoured: deeper than the scanned tip, nothing counts.
    let err = estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        Some(fixture::FUNDED_TIP_HEIGHT as u64),
    )
    .expect_err("no output is that deeply confirmed");
    assert!(err.to_string().starts_with("Wallet Error: "), "got {err}");
}

#[test]
fn fee_for_more_outputs_is_higher() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _funded = with_funded_wallet();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (one, three) = runtime.block_on(async {
        // A node with no fee stats: the estimator uses its default rates.
        let server = wiremock::MockServer::start().await;
        let estimate = |outputs| {
            estimate_transaction_fee_for_outputs(
                1_000_000,
                outputs,
                FeePriority::Medium,
                server.uri(),
                FUNDED.to_string(),
            )
        };
        (
            estimate(1).await.expect("one output"),
            estimate(3).await.expect("three outputs"),
        )
    });
    assert!(one.estimated_fee > 0);
    assert!(
        three.estimated_fee > one.estimated_fee,
        "three outputs ({}) must cost more than one ({})",
        three.estimated_fee,
        one.estimated_fee
    );
    assert_eq!(three.total_amount_required, 1_000_000 + three.estimated_fee);
}

#[test]
fn send_transaction_multi_pays_every_recipient() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let stages = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            let details = funded_send(
                &funded,
                vec![
                    SendRecipient {
                        address: funded.recipient.clone(),
                        amount: 100_000,
                        payment_id: Some("first".to_string()),
                    },
                    SendRecipient {
                        address: funded.recipient.clone(),
                        amount: 200_000,
                        payment_id: None,
                    },
                ],
                false,
                server.uri(),
            );
            let stages = stages.clone();
            send_transaction_multi_with_handler(details, move |event| {
                stages.lock().unwrap().push(event.stage)
            })
            .await
        })
        .expect("send_transaction_multi");

    // The mock node answers 404: the send is recorded but not accepted yet.
    assert!(!sent.accepted_by_node, "the mock node accepted nothing");
    assert!(sent.note.is_some(), "the user is told not to resend");
    assert!(sent.node_reason.is_some());
    let sent = sent.transaction;
    assert!(!sent.id.is_empty());
    assert!(matches!(
        stages.lock().unwrap().last(),
        Some(TransactionStage::Completed)
    ));
    // The spent inputs stay locked until the scan sees the send mined.
    let balance = get_balance(FUNDED.to_string()).expect("balance");
    assert!(balance.locked > 0, "inputs locked by the send");
}

#[test]
fn send_all_sweeps_the_quoted_amount() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let estimate = estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect("estimate_send_all_fee");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            let details = funded_send(
                &funded,
                vec![SendRecipient {
                    address: funded.recipient.clone(),
                    amount: 0,
                    payment_id: None,
                }],
                true,
                server.uri(),
            );
            send_transaction_multi_with_handler(details, |_| {}).await
        })
        .expect("send-all")
        .transaction;

    // The history shows what left the wallet: the quoted payment plus the quoted fee.
    assert_eq!(
        sent.amount,
        estimate.amount_after_fee + estimate.fee,
        "pays and charges what was quoted"
    );
    let balance = get_balance(FUNDED.to_string()).expect("balance");
    assert_eq!(balance.available, 0, "every output is spent");
}

/// A key-imported wallet has no seed words: `send_transaction_multi` with empty
/// `seed_words` signs with its stored keys. The frozen send still needs seed words.
#[test]
fn key_imported_wallet_sends_with_its_stored_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let (dir, db_path) = fixture::materialize_fixture_db().expect("build fixture DB");
    initialize_database(db_path.to_string_lossy().to_string()).expect("initialize_database");
    let _db = DbGuard { _dir: dir };

    import_wallet_from_keys(
        "keyed".to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        IMPORT_SPEND_KEY_HEX.to_string(),
        fixture::BIRTHDAY,
        "pw".to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("import_wallet_from_keys");
    fixture::add_spendable_outputs(&db_path, "keyed", "pw", &FUNDED_VALUES).expect("fund");
    let recipient = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(String::new()),
        Some(TariNetwork::Esmeralda),
    )
    .expect("fixture address");

    let details = |passphrase: &str, base_url: String| MultiSendTransactionDetails {
        seed_words: Vec::new(),
        passphrase: Some(passphrase.to_string()),
        network: Some(TariNetwork::Esmeralda),
        base_url: Some(base_url),
        wallet_name: "keyed".to_string(),
        recipients: vec![SendRecipient {
            address: recipient.clone(),
            amount: 100_000,
            payment_id: None,
        }],
        send_all: false,
        confirmation_window: None,
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(send_transaction_multi_with_handler(
            details("wrong", "http://127.0.0.1:1".to_string()),
            |_| {},
        ))
        .expect_err("a wrong passphrase must be refused");
    assert_eq!(err.to_string(), "Invalid Passphrase");
    assert_eq!(get_balance("keyed".to_string()).unwrap().locked, 0);

    let frozen = SendTransactionDetails {
        seed_words: Vec::new(),
        passphrase: Some("pw".to_string()),
        network: Some(TariNetwork::Esmeralda),
        base_url: None,
        wallet_name: "keyed".to_string(),
        recipient_address: recipient.clone(),
        amount: 100_000,
        payment_id: None,
        confirmation_window: None,
    };
    let err = runtime
        .block_on(send_transaction_with_handler(frozen, |_| {}))
        .expect_err("the frozen send needs seed words");
    assert!(
        err.to_string().starts_with("Invalid Seed Words: "),
        "got {err}"
    );

    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            send_transaction_multi_with_handler(details("pw", server.uri()), |_| {}).await
        })
        .expect("a key-imported wallet sends with its stored keys");
    assert!(!sent.transaction.id.is_empty());
    assert!(get_balance("keyed".to_string()).unwrap().locked > 0);
}

/// Send 100_000 to the fixture wallet through a mock node answering `submit` with
/// `accepted`, or (`None`) a node that answers nothing.
fn send_via_mock_node(
    funded: &FundedWallet,
    runtime: &tokio::runtime::Runtime,
    accepted: Option<bool>,
) -> rust_lib_flutter_rust_wallet::api::send_transaction::SendResultDto {
    runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            if let Some(accepted) = accepted {
                wiremock::Mock::given(wiremock::matchers::method("POST"))
                    .and(wiremock::matchers::path("/json_rpc"))
                    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": "1",
                            "result": {
                                "accepted": accepted,
                                "rejection_reason": "None",
                                "is_synced": true
                            }
                        }),
                    ))
                    .mount(&server)
                    .await;
            }
            let details = funded_send(
                funded,
                vec![SendRecipient {
                    address: funded.recipient.clone(),
                    amount: 100_000,
                    payment_id: None,
                }],
                false,
                server.uri(),
            );
            send_transaction_multi_with_handler(details, |_| {}).await
        })
        .expect("send")
}

/// Record that the node refused the send's last rebroadcasts until it stalled, as
/// the scan's transaction monitor does.
fn stall(funded: &FundedWallet, transaction_id: &str) {
    let pool = minotari_wallet::db::init_db(funded.db_path.clone()).expect("pool");
    let tx_id: u64 = transaction_id.parse().expect("numeric tx id");
    minotari_wallet::db::record_failed_broadcast_attempt(
        &pool.get().expect("conn"),
        tari_common_types::transaction::TxId::from(tx_id),
        minotari_wallet::transactions::monitor::MAX_BROADCAST_ATTEMPTS,
        "not accepted",
    )
    .expect("record attempts");
}

const NOT_STALLED: &str =
    "Wallet Error: The send has not stalled yet; keep scanning so the wallet rebroadcasts it";

/// A send the node never accepted is abandoned only once it has stalled; then its
/// inputs are released. A running scan is stopped first.
#[test]
fn abandon_send_releases_a_stalled_send() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let sent = send_via_mock_node(&funded, &runtime, None);
    assert!(!sent.accepted_by_node);
    assert_eq!(
        spendable_outputs(&funded),
        Some(FUNDED_VALUES.len() - 1),
        "the send holds one input"
    );
    let tx_id = sent.transaction.id;
    let abandon = |passphrase: &str, id: &str| {
        runtime.block_on(abandon_send(
            FUNDED.to_string(),
            passphrase.to_string(),
            id.to_string(),
        ))
    };

    assert_eq!(
        abandon("wrong", &tx_id).unwrap_err().to_string(),
        "Invalid Passphrase"
    );
    assert_eq!(
        abandon(FUNDED_PASSPHRASE, "999").unwrap_err().to_string(),
        "Wallet Error: No send 999 in this account"
    );
    assert_eq!(
        abandon(FUNDED_PASSPHRASE, &tx_id).unwrap_err().to_string(),
        NOT_STALLED
    );
    assert!(get_balance(FUNDED.to_string()).unwrap().locked > 0);

    stall(&funded, &tx_id);

    // A continuous scan is running; abandon_send stops it first.
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let (scan, _server) = runtime.block_on(async {
        let server = mock_chain_node().await;
        let config = ScanConfiguration {
            continuous: true,
            ..rescan_config("", server.uri())
        };
        let scan = tokio::spawn(start_scan_v2_with_handler(config, move |_| {
            let _ = seen_tx.send(());
            Ok(())
        }));
        seen_rx.recv().await.expect("the scan is running");
        (scan, server)
    });

    let note = abandon(FUNDED_PASSPHRASE, &tx_id).expect("abandon_send");
    loop {
        match seen_rx.try_recv() {
            Ok(()) => continue,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                panic!("the scan was still running after abandon_send")
            }
        }
    }
    runtime
        .block_on(scan)
        .unwrap()
        .expect("a stopped scan returns Ok");

    assert!(note.contains("inputs are released"), "got {note}");
    assert_eq!(get_balance(FUNDED.to_string()).unwrap().locked, 0);
    assert_eq!(
        spendable_outputs(&funded),
        Some(FUNDED_VALUES.len()),
        "every input is spendable again"
    );
}

/// A send the node accepted is refused while it has not stalled.
#[test]
fn abandon_send_refuses_an_accepted_send_that_has_not_stalled() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let sent = send_via_mock_node(&funded, &runtime, Some(true));
    assert!(sent.accepted_by_node);
    let locked = get_balance(FUNDED.to_string()).unwrap().locked;
    assert!(locked > 0);

    let err = runtime
        .block_on(abandon_send(
            FUNDED.to_string(),
            FUNDED_PASSPHRASE.to_string(),
            sent.transaction.id,
        ))
        .expect_err("an accepted send that has not stalled cannot be abandoned");
    assert_eq!(err.to_string(), NOT_STALLED);
    assert_eq!(
        get_balance(FUNDED.to_string()).unwrap().locked,
        locked,
        "nothing was unlocked"
    );
}

/// A view-only wallet sending with its stored keys is refused before anything is
/// locked.
#[test]
fn view_only_wallet_cannot_send_with_stored_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let details = MultiSendTransactionDetails {
        seed_words: Vec::new(),
        passphrase: Some(String::new()),
        network: Some(TariNetwork::Esmeralda),
        base_url: Some("http://127.0.0.1:1".to_string()),
        wallet_name: fixture::WALLET_NAME.to_string(),
        recipients: vec![SendRecipient {
            address: funded.recipient.clone(),
            amount: 1_000,
            payment_id: None,
        }],
        send_all: false,
        confirmation_window: None,
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let err = runtime
        .block_on(send_transaction_multi_with_handler(details, |_| {}))
        .expect_err("a view-only wallet cannot send");
    assert_eq!(
        err.to_string(),
        "Signing Error: View-only wallets cannot send"
    );
    let balance = get_balance(fixture::WALLET_NAME.to_string()).expect("balance");
    assert_eq!(balance.locked, 0);
    assert_eq!(balance.total, fixture::GOLDEN_TOTAL);
}

/// The frozen single-recipient send still resolves `Ok` when the node does not
/// accept the broadcast: the send is recorded and must not be resent.
#[test]
fn frozen_send_resolves_ok_when_the_node_does_not_accept() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            let details = SendTransactionDetails {
                seed_words: funded.seed_words.clone(),
                passphrase: Some(FUNDED_PASSPHRASE.to_string()),
                network: Some(TariNetwork::Esmeralda),
                base_url: Some(server.uri()),
                wallet_name: FUNDED.to_string(),
                recipient_address: funded.recipient.clone(),
                amount: 100_000,
                payment_id: None,
                confirmation_window: None,
            };
            send_transaction_with_handler(details, |_| {}).await
        })
        .expect("the frozen send resolves Ok");
    assert!(!sent.id.is_empty());
}

// ---------------------------------------------------------------------------
// start_scan_v2 against a mock chain.
// ---------------------------------------------------------------------------

/// A mock base node whose chain runs to `MOCK_TIP` with no wallet outputs. Its tip
/// matches the funded wallet's scanned tip, so a plain scan of that wallet resumes
/// with nothing to do and only a rollback removes its outputs.
struct MockChain;

const MOCK_TIP: u64 = fixture::FUNDED_TIP_HEIGHT as u64;

fn mock_block_hash(height: u64) -> Vec<u8> {
    fixture::chain_block_hash(height)
}

fn mock_query(request: &wiremock::Request, name: &str) -> String {
    request
        .url
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.to_string())
        .expect("query parameter")
}

impl wiremock::Respond for MockChain {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        use tari_transaction_components::rpc::models::{
            BlockUtxoInfo, SyncUtxosByBlockResponseV0, SyncUtxosByBlockResponseV1,
        };
        use tari_utilities::hex::from_hex;
        use wiremock::ResponseTemplate;

        match request.url.path() {
            "/get_tip_info" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "metadata": {
                    "best_block_height": MOCK_TIP,
                    "best_block_hash": mock_block_hash(MOCK_TIP),
                    "accumulated_difficulty": "0",
                    "pruned_height": 0,
                    "timestamp": 0,
                }
            })),
            "/get_height_at_time" => ResponseTemplate::new(200).set_body_json(0u64),
            "/get_header_by_height" => {
                let height: u64 = mock_query(request, "height").parse().expect("height");
                if height > MOCK_TIP {
                    return ResponseTemplate::new(404);
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "height": height,
                    "hash": mock_block_hash(height),
                    "timestamp": 1_700_000_000u64,
                }))
            }
            "/sync_utxos_by_block" => {
                let start_hash = from_hex(&mock_query(request, "start_header_hash")).expect("hex");
                let start = u64::from_be_bytes(start_hash[0..8].try_into().expect("8 bytes"));
                let limit: u64 = mock_query(request, "limit").parse().expect("limit");
                let last = (start + limit - 1).min(MOCK_TIP);
                let blocks = (start..=last)
                    .map(|height| BlockUtxoInfo {
                        header_hash: mock_block_hash(height),
                        height,
                        outputs: Vec::new(),
                        inputs: Vec::new(),
                        mined_timestamp: 1_700_000_000,
                    })
                    .collect();
                let next_header_to_scan = if last < MOCK_TIP {
                    mock_block_hash(last + 1)
                } else {
                    Vec::new()
                };
                ResponseTemplate::new(200).set_body_json(SyncUtxosByBlockResponseV1::from(
                    SyncUtxosByBlockResponseV0 {
                        blocks,
                        has_next_page: false,
                        next_header_to_scan,
                    },
                ))
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

#[test]
fn start_scan_v2_scans_to_the_tip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _db = with_fixture_db();

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(async {
            let server = mock_chain_node().await;
            let sink = events.clone();
            start_scan_v2_with_handler(rescan_config("", server.uri()), move |event| {
                sink.lock().unwrap().push(event);
                Ok(())
            })
            .await
        })
        .expect("start_scan_v2");

    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ScanEventV2Dto::Progress {
                tip_height: MOCK_TIP,
                ..
            }
        )),
        "progress carries the tip, got {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(ScanEventV2Dto::Completed {
                final_height: MOCK_TIP,
                ..
            })
        ),
        "got {events:?}"
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
