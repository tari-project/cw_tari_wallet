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
//! Every API funnels through the process-global database path and the wallet
//! sessions (`set_database_path` / `unlock_wallet`). These tests therefore mutate
//! shared global state and MUST run sequentially — they are serialized via
//! [`SERIAL`] and each one locks the wallets it unlocked on exit.

// Spawning a scan needs its (deep) minotari future proven `Send`; see `src/lib.rs`.
#![recursion_limit = "256"]

use std::cell::RefCell;
use std::sync::Mutex;

use minotari_wallet_ffi::api::address::get_address;
use minotari_wallet_ffi::api::balance::get_balance;
use minotari_wallet_ffi::api::db::{lock_wallet, set_database_path, unlock_wallet};
use minotari_wallet_ffi::api::fee::{
    estimate_send_all_fee, estimate_send_all_fee_at, estimate_transaction_fee_for_outputs,
    get_fee_per_gram, FeePriority,
};
use minotari_wallet_ffi::api::network::current_network;
use minotari_wallet_ffi::api::network::TariNetwork;
use minotari_wallet_ffi::api::scanner::{
    create_scan_handle, rescan_with_handler, start_scan_with_handler, stop_scan, ScanConfiguration,
    ScanEventDto, ScanPriority,
};
use minotari_wallet_ffi::api::send_transaction::{
    abandon_send, get_abandon_warnings, send_transaction_at_fee_with_handler,
    send_transaction_with_handler, AbandonWarningKind, MultiSendTransactionDetails, SendRecipient,
    SendResultDto, SendTransactionEvent, TransactionStage,
};
use minotari_wallet_ffi::api::signing::{sign_message, verify_message};
use minotari_wallet_ffi::api::transactions::get_transactions;
use minotari_wallet_ffi::api::wallet::{
    change_passphrase, import_wallet_from_keys, list_wallets, restore_wallet,
};
use verify::fixture;

/// Serializes the DB-touching tests: they all install the single process-global
/// DB pool, so running them in parallel would race the shared slot.
static SERIAL: Mutex<()> = Mutex::new(());

/// RAII guard: keeps the fixture temp dir alive for the test's duration and locks
/// every wallet it unlocked on drop — even if the test panics on a failed assertion
/// before reaching an explicit teardown — which closes the shared pool.
struct DbGuard {
    _dir: tempfile::TempDir,
    db_path: std::path::PathBuf,
    unlocked: RefCell<Vec<String>>,
}

impl DbGuard {
    fn unlock(&self, wallet_name: &str, passphrase: &str) -> anyhow::Result<()> {
        unlock_wallet(
            self.db_path.to_string_lossy().to_string(),
            wallet_name.to_string(),
            passphrase.to_string(),
        )?;
        self.unlocked.borrow_mut().push(wallet_name.to_string());
        Ok(())
    }
}

impl Drop for DbGuard {
    fn drop(&mut self) {
        // Best-effort teardown on a fresh thread (a test may drop this inside its own
        // runtime); ignore errors so teardown never masks the original panic.
        let wallets = std::mem::take(self.unlocked.get_mut());
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Runtime::new() else {
                return;
            };
            for wallet in wallets {
                let _ = runtime.block_on(lock_wallet(wallet));
            }
        })
        .join();
    }
}

/// Materialize a fresh fixture DB in a temp dir and point the database path at it.
/// No wallet is unlocked yet. The caller holds the serialization lock for the test.
fn with_fixture_db() -> DbGuard {
    let (dir, db_path) = fixture::materialize_fixture_db().expect("build fixture DB");
    set_database_path(db_path.to_string_lossy().to_string()).expect("set_database_path");
    DbGuard {
        _dir: dir,
        db_path,
        unlocked: RefCell::new(Vec::new()),
    }
}

/// The fixture wallet's address, unlocking it (empty passphrase) first.
fn fixture_address(db: &DbGuard) -> String {
    db.unlock(fixture::WALLET_NAME, "")
        .expect("unlock the fixture");
    get_address(
        fixture::WALLET_NAME.to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("fixture address")
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
    let db = with_fixture_db();

    let err = get_address(
        fixture::WALLET_NAME.to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect_err("a locked wallet has no address");
    assert_eq!(err.to_string(), "Wallet is locked");

    // Unlocked (the fixture account was imported with an empty passphrase), encoded
    // for the fixture's network.
    let address = fixture_address(&db);

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
            minotari_wallet_ffi::api::transactions::DisplayedTransactionDirection::Incoming
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

/// A send needs the wallet's session: while locked it is refused before any UTXO is
/// reserved, and leaves the balance untouched.
#[test]
fn send_while_locked_is_refused_without_touching_the_balance() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(lock_wallet(FUNDED.to_string()))
        .expect("lock_wallet");

    let before = get_balance(FUNDED.to_string()).expect("balance before");
    let details = funded_send(
        &funded,
        vec![SendRecipient {
            address: funded.recipient.clone(),
            amount: 1_000,
            payment_id: None,
        }],
        false,
        "http://127.0.0.1:1".to_string(),
    );
    let err = runtime
        .block_on(send_transaction_with_handler(details, |_| {}))
        .expect_err("a locked wallet cannot send");
    assert_eq!(err.to_string(), "Wallet is locked");

    let after = get_balance(FUNDED.to_string()).expect("balance after");
    assert_eq!(
        after.locked, 0,
        "no UTXO may be left locked by a refused send"
    );
    assert_eq!(after.available, before.available);
    assert_eq!(after.total, before.total);
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
    let db = with_fixture_db();

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
    db.unlock("imported", "pw").expect("unlock");
    let signature =
        sign_message("imported".to_string(), "hi".into()).expect("a key-imported wallet can sign");
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
fn sign_message_refuses_a_locked_or_view_only_wallet() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let err = sign_message(fixture::WALLET_NAME.to_string(), "m".into())
        .expect_err("a locked wallet cannot sign");
    assert_eq!(err.to_string(), "Wallet is locked");

    db.unlock(fixture::WALLET_NAME, "").expect("unlock");
    let err = sign_message(fixture::WALLET_NAME.to_string(), "m".into())
        .expect_err("a view-only wallet cannot sign");
    assert_eq!(
        err.to_string(),
        "Signing Error: View-only wallets cannot sign messages"
    );

    let err = sign_message("missing".to_string(), "m".into())
        .expect_err("an unknown wallet must be refused");
    assert_eq!(err.to_string(), "No accounts found for this wallet");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(lock_wallet(fixture::WALLET_NAME.to_string()))
        .expect("lock_wallet");
    let err = db
        .unlock(fixture::WALLET_NAME, "wrong")
        .expect_err("a wrong passphrase is refused");
    assert_eq!(err.to_string(), "Invalid Passphrase");
}

#[test]
fn change_passphrase_reencrypts_the_wallet() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let before = fixture_address(&db);
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    // A continuous scan is running. A wrong old passphrase leaves it running; a
    // right one stops it before the change.
    runtime.block_on(async {
        let server = mock_chain_node().await;
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let config = ScanConfiguration {
            continuous: true,
            ..scan_config(fixture::WALLET_NAME, server.uri())
        };
        let handle = create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle");
        let scan = tokio::spawn(start_scan_with_handler(handle, config, move |event| {
            let _ = seen_tx.send(event);
            Ok(())
        }));
        seen_rx.recv().await.expect("the scan is running");

        let err = change_passphrase(
            fixture::WALLET_NAME.to_string(),
            "wrong".to_string(),
            "new".to_string(),
        )
        .await
        .expect_err("a wrong old passphrase must be refused");
        assert_eq!(err.to_string(), "Invalid Passphrase");
        assert!(
            !scan.is_finished(),
            "a refused change leaves the scan running"
        );

        change_passphrase(
            fixture::WALLET_NAME.to_string(),
            String::new(),
            "new".to_string(),
        )
        .await
        .expect("change_passphrase");
        // The scan had ended its stream (and dropped its callback) before the change.
        let mut last = None;
        loop {
            match seen_rx.try_recv() {
                Ok(event) => last = Some(event),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    panic!("the scan was still running after change_passphrase")
                }
            }
        }
        assert!(
            matches!(last, Some(ScanEventDto::Cancelled)),
            "got {last:?}"
        );
        scan.await.unwrap().expect("a stopped scan returns Ok");
    });
    // The session survives the change.
    let address = || {
        get_address(
            fixture::WALLET_NAME.to_string(),
            Some(TariNetwork::Esmeralda),
        )
    };
    assert_eq!(address().expect("still unlocked"), before);
    // Even while unlocked, the old passphrase no longer opens it.
    assert_eq!(
        db.unlock(fixture::WALLET_NAME, "").unwrap_err().to_string(),
        "Invalid Passphrase",
        "the old passphrase is checked even while unlocked"
    );

    let locked = runtime
        .block_on(lock_wallet(fixture::WALLET_NAME.to_string()))
        .expect("lock_wallet");
    assert!(locked, "the refused unlock left no extra count");
    assert_eq!(
        db.unlock(fixture::WALLET_NAME, "").unwrap_err().to_string(),
        "Invalid Passphrase",
        "the old passphrase no longer opens it"
    );
    db.unlock(fixture::WALLET_NAME, "new")
        .expect("the new passphrase opens it");
    assert_eq!(address().expect("unlocked again"), before);
}

#[test]
fn send_all_estimate_errors_when_nothing_is_spendable() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    let recipient = fixture_address(&db);

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

fn scan_config(wallet_name: &str, base_url: String) -> ScanConfiguration {
    ScanConfiguration {
        wallet_name: wallet_name.to_string(),
        base_url,
        batch_size: 10,
        continuous: false,
        poll_interval_seconds: 1,
        required_confirmations: 3,
        priority: ScanPriority::Foreground,
        processing_threads: None,
        event_buffer: None,
        deadline_seconds: None,
        max_blocks: None,
    }
}

/// A rescan of a locked wallet is refused and rolls nothing back.
#[test]
fn rescan_of_a_locked_wallet_changes_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(lock_wallet(FUNDED.to_string()))
        .expect("lock_wallet");

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let err = runtime
        .block_on(rescan_with_handler(
            create_scan_handle(FUNDED.to_string()).expect("handle"),
            5,
            scan_config(FUNDED, "http://127.0.0.1:1".to_string()),
            move |event| {
                sink.lock().unwrap().push(event);
                Ok(())
            },
        ))
        .expect_err("a locked wallet cannot rescan");
    assert_eq!(err.to_string(), "Wallet is locked");
    assert!(matches!(
        events.lock().unwrap().as_slice(),
        [ScanEventDto::Error(_)]
    ));
    assert_eq!(
        spendable_outputs(&funded),
        Some(FUNDED_VALUES.len()),
        "a refused rescan rolls nothing back"
    );
}

/// Mount the [`MockChain`] responder on a fresh mock node.
async fn mock_chain_node() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(MockChain::default())
        .mount(&server)
        .await;
    server
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
                create_scan_handle(FUNDED.to_string()).expect("handle"),
                5,
                scan_config(FUNDED, server.uri()),
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
            Some(ScanEventDto::Completed {
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
    db: DbGuard,
    recipient: String,
}

/// The fixture DB plus a restored seed-words wallet holding [`FUNDED_VALUES`], both
/// unlocked. The recipient is the fixture's own (view-only) wallet.
fn with_funded_wallet() -> FundedWallet {
    let db = with_fixture_db();
    let db_path = db.db_path.clone();

    restore_wallet(
        FUNDED.to_string(),
        fixture::random_seed_words(),
        FUNDED_PASSPHRASE.to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("restore_wallet");
    fixture::add_spendable_outputs(&db_path, FUNDED, FUNDED_PASSPHRASE, &FUNDED_VALUES)
        .expect("add outputs");
    db.unlock(FUNDED, FUNDED_PASSPHRASE)
        .expect("unlock the funded wallet");

    let recipient = fixture_address(&db);
    FundedWallet { db, recipient }
}

fn funded_send(
    _funded: &FundedWallet,
    recipients: Vec<SendRecipient>,
    send_all: bool,
    base_url: String,
) -> MultiSendTransactionDetails {
    MultiSendTransactionDetails {
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

/// A mock node answering fee-per-gram `stats` (`(min, avg)` per block; empty: no stats).
async fn node_with_fee_stats(stats: &[(u64, u64)]) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    if !stats.is_empty() {
        let stats: Vec<_> = stats
            .iter()
            .enumerate()
            .map(|(order, (min, avg))| {
                serde_json::json!({
                    "order": order, "min_fee_per_gram": min, "avg_fee_per_gram": avg,
                    "max_fee_per_gram": avg,
                })
            })
            .collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/get_mempool_fee_per_gram_stats"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "stats": stats })),
            )
            .mount(&server)
            .await;
    }
    server
}

/// A node rate above the cap is capped before inputs are selected: a wallet that can pay
/// at the cap, but not at the node's rate, is quoted exactly as at a node saying the cap.
#[test]
fn node_fee_rates_are_capped_before_inputs_are_selected() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let _funded = with_funded_wallet();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let estimate_with_node_rate = |rate: u64| {
        runtime.block_on(async {
            let server = node_with_fee_stats(&[(rate, rate)]).await;
            estimate_transaction_fee_for_outputs(
                5_000_000,
                1,
                FeePriority::Fast,
                server.uri(),
                FUNDED.to_string(),
            )
            .await
        })
    };
    let at_cap = estimate_with_node_rate(100).expect("affordable at the cap");
    assert_eq!(at_cap.fee_per_gram, 100);
    assert_eq!(
        at_cap.input_count, 3,
        "5 T of 1, 2 and 3 T needs every output"
    );
    // 1 T a gram: the 6 T wallet could never pay this.
    let over = estimate_with_node_rate(1_000_000).expect("quoted at the cap");
    assert_eq!(over.fee_per_gram, 100);
    assert_eq!(over.input_count, at_cap.input_count);
    assert_eq!(over.estimated_fee, at_cap.estimated_fee);
    assert_eq!(over.total_amount_required, 5_000_000 + at_cap.estimated_fee);
}

/// Upstream-drift tripwire: when the node's rates are in order and Medium and Fast are
/// within 5..=100, the bridge's estimate is exactly minotari's
/// `FeeEstimator::estimate_fees` for that priority.
#[test]
fn fee_estimates_below_the_cap_match_minotari() {
    use minotari_wallet::transactions::fee_estimator::{
        FeeEstimator, FeePriority as LibFeePriority,
    };

    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let pool = minotari_wallet::db::init_db(funded.db.db_path.clone()).expect("pool");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        // No stats (minotari's defaults), a zero Slow (its minimum), one block, three blocks.
        for stats in [
            vec![],
            vec![(0, 5), (0, 5)],
            vec![(4, 30)],
            vec![(9, 100), (6, 40), (2, 12)],
        ] {
            let server = node_with_fee_stats(&stats).await;
            for (amount, outputs) in [(1_000, 1), (2_500_000, 2), (5_000_000, 1)] {
                let upstream = FeeEstimator::new(pool.clone(), server.uri())
                    .estimate_fees(
                        FUNDED,
                        tari_transaction_components::MicroMinotari(amount),
                        outputs,
                        3,
                        None,
                    )
                    .await
                    .expect("upstream estimate");
                for priority in [FeePriority::Slow, FeePriority::Medium, FeePriority::Fast] {
                    let ours = estimate_transaction_fee_for_outputs(
                        amount,
                        outputs as u32,
                        priority,
                        server.uri(),
                        FUNDED.to_string(),
                    )
                    .await
                    .expect("estimate");
                    let theirs = upstream
                        .iter()
                        .find(|e| e.priority == LibFeePriority::from(priority))
                        .expect("every priority");
                    let case = format!("{stats:?} {amount} x{outputs} {priority:?}");
                    assert_eq!(ours.fee_per_gram, theirs.fee_per_gram.0, "{case}");
                    assert_eq!(ours.estimated_fee, theirs.estimated_fee.0, "{case}");
                    assert_eq!(
                        ours.total_amount_required, theirs.total_amount_required.0,
                        "{case}"
                    );
                    assert_eq!(ours.input_count, theirs.input_count, "{case}");
                }
            }
        }
    });
}

#[test]
fn send_transaction_pays_every_recipient() {
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
            send_transaction_with_handler(details, move |event| {
                stages.lock().unwrap().push(event.stage)
            })
            .await
        })
        .expect("send_transaction");

    // The mock node answers 404: the send is recorded but not accepted yet.
    assert!(!sent.accepted_by_node, "the mock node accepted nothing");
    assert!(sent.note.is_some(), "the user is told not to resend");
    assert!(sent.node_reason.is_some());
    let sent = sent.transaction;
    assert!(!sent.id.is_empty());
    assert!(matches!(
        stages.lock().unwrap().last(),
        Some(TransactionStage::Broadcasted)
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
            send_transaction_with_handler(details, |_| {}).await
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

/// A send-all at a chosen rate pays exactly its quote at that rate, which scales with
/// the rate (same inputs and outputs, so the same weight).
#[test]
fn send_all_at_a_rate_sweeps_the_quoted_amount() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let quote = |fee_per_gram| {
        estimate_send_all_fee_at(
            FUNDED.to_string(),
            funded.recipient.clone(),
            None,
            Some(TariNetwork::Esmeralda),
            None,
            fee_per_gram,
        )
        .expect("estimate_send_all_fee_at")
    };
    let default = estimate_send_all_fee(
        FUNDED.to_string(),
        funded.recipient.clone(),
        None,
        Some(TariNetwork::Esmeralda),
        None,
    )
    .expect("estimate_send_all_fee");
    assert_eq!(quote(5).fee, default.fee, "the default rate is 5");
    let estimate = quote(20);
    assert_eq!(estimate.fee, 4 * default.fee);
    assert_eq!(estimate.input_count, 3);

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (sent, kernel_fee) = runtime.block_on(async {
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
        let sent = send_transaction_at_fee_with_handler(details, 20, |_| {}).await;
        (sent, broadcast_kernel_fee(&server).await)
    });
    let sent = sent.expect("send-all at 20").transaction;

    assert_eq!(kernel_fee, estimate.fee, "built at 20");
    assert_eq!(sent.amount, estimate.amount_after_fee + estimate.fee);
    let balance = get_balance(FUNDED.to_string()).expect("balance");
    assert_eq!(balance.available, 0, "every output is spent");
}

/// The fee in the kernel of the transaction broadcast to the mock node.
async fn broadcast_kernel_fee(server: &wiremock::MockServer) -> u64 {
    let requests = server.received_requests().await.expect("recorded requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests.first().expect("a broadcast").body).expect("json");
    body["params"]["transaction"]["body"]["kernels"][0]["fee"]
        .as_u64()
        .expect("kernel fee")
}

/// A normal send is built at the rate it is given: the same send from the same wallet
/// state pays 4x the fee at 20 µT/gram as at 5.
#[test]
fn a_normal_send_is_built_at_the_given_rate() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let kernel_fee_at = |rate: u64| {
        let funded = with_funded_wallet();
        runtime.block_on(async {
            let server = wiremock::MockServer::start().await;
            let details = funded_send(
                &funded,
                vec![SendRecipient {
                    address: funded.recipient.clone(),
                    amount: 100_000,
                    payment_id: None,
                }],
                false,
                server.uri(),
            );
            send_transaction_at_fee_with_handler(details, rate, |_| {})
                .await
                .expect("send");
            broadcast_kernel_fee(&server).await
        })
    };
    let at_default = kernel_fee_at(5);
    let at_twenty = kernel_fee_at(20);
    assert!(at_default > 0);
    assert_eq!(at_twenty, 4 * at_default);
}

/// The rate alone is the estimate's rate: capped, Slow floored at minotari's minimum and
/// Medium and Fast at 5, kept in order, and minotari's defaults when the node has no
/// statistics.
#[test]
fn the_fee_per_gram_is_the_capped_node_rate() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        let rates = |stats: &'static [(u64, u64)]| async move {
            let server = node_with_fee_stats(stats).await;
            let mut rates = Vec::new();
            for priority in [FeePriority::Slow, FeePriority::Medium, FeePriority::Fast] {
                rates.push(
                    get_fee_per_gram(priority, server.uri())
                        .await
                        .expect("rate"),
                );
            }
            rates
        };
        assert_eq!(rates(&[]).await, [1, 5, 10], "no stats");
        assert_eq!(rates(&[(0, 0)]).await, [1, 5, 5], "zeros");
        assert_eq!(rates(&[(2, 2), (50, 50)]).await, [50, 50, 50], "inverted");
        assert_eq!(rates(&[(9, 100), (6, 40), (2, 12)]).await, [2, 40, 100]);
        assert_eq!(
            rates(&[(5_000, 1_000_000)]).await,
            [100, 100, 100],
            "capped"
        );
        let err = get_fee_per_gram(FeePriority::Fast, "not a url".to_string())
            .await
            .expect_err("bad url");
        assert_eq!(err.to_string(), "Failed to get the fee per gram");
    });
}

/// A rate of 0 or above the cap is refused before any UTXO is reserved, by the send
/// and by the send-all quote.
#[test]
fn an_out_of_range_rate_is_refused_without_locking_anything() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    for (rate, message) in [
        (
            0,
            "Invalid fee per gram: 0 (must be 1 to 100 microTari per gram)",
        ),
        (
            101,
            "Invalid fee per gram: 101 (must be 1 to 100 microTari per gram)",
        ),
    ] {
        let details = funded_send(
            &funded,
            vec![SendRecipient {
                address: funded.recipient.clone(),
                amount: 1_000,
                payment_id: None,
            }],
            false,
            "http://127.0.0.1:1".to_string(),
        );
        let err = runtime
            .block_on(send_transaction_at_fee_with_handler(details, rate, |_| {}))
            .expect_err("refused rate");
        assert_eq!(err.to_string(), message);
        let err = estimate_send_all_fee_at(
            FUNDED.to_string(),
            funded.recipient.clone(),
            None,
            Some(TariNetwork::Esmeralda),
            None,
            rate,
        )
        .expect_err("refused rate");
        assert_eq!(err.to_string(), message);
    }
    let balance = get_balance(FUNDED.to_string()).expect("balance");
    assert_eq!(balance.locked, 0, "nothing reserved");
}

/// A key-imported wallet has no seed words: once unlocked it sends with its stored
/// keys.
#[test]
fn key_imported_wallet_sends_with_its_stored_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();

    import_wallet_from_keys(
        "keyed".to_string(),
        IMPORT_VIEW_KEY_HEX.to_string(),
        IMPORT_SPEND_KEY_HEX.to_string(),
        fixture::BIRTHDAY,
        "pw".to_string(),
        Some(TariNetwork::Esmeralda),
    )
    .expect("import_wallet_from_keys");
    fixture::add_spendable_outputs(&db.db_path, "keyed", "pw", &FUNDED_VALUES).expect("fund");
    let recipient = fixture_address(&db);

    let details = |base_url: String| MultiSendTransactionDetails {
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
        .block_on(send_transaction_with_handler(
            details("http://127.0.0.1:1".to_string()),
            |_| {},
        ))
        .expect_err("a locked wallet must be refused");
    assert_eq!(err.to_string(), "Wallet is locked");
    assert_eq!(get_balance("keyed".to_string()).unwrap().locked, 0);

    db.unlock("keyed", "pw").expect("unlock");
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            send_transaction_with_handler(details(server.uri()), |_| {}).await
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
) -> SendResultDto {
    let submit = accepted.map(|accepted| (accepted, "None"));
    send_via_mock_node_with_events(funded, runtime, submit).1
}

/// [`send_via_mock_node`] answering `submit` with `(accepted, rejection_reason)`,
/// returning every streamed event as well as the resolved result.
fn send_via_mock_node_with_events(
    funded: &FundedWallet,
    runtime: &tokio::runtime::Runtime,
    submit: Option<(bool, &str)>,
) -> (Vec<SendTransactionEvent>, SendResultDto) {
    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sent = runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            if let Some((accepted, rejection_reason)) = submit {
                wiremock::Mock::given(wiremock::matchers::method("POST"))
                    .and(wiremock::matchers::path("/json_rpc"))
                    .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": "1",
                            "result": {
                                "accepted": accepted,
                                "rejection_reason": rejection_reason,
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
            let sink = events.clone();
            send_transaction_with_handler(details, move |event| sink.lock().unwrap().push(event))
                .await
        })
        .expect("send");
    let events = std::mem::take(&mut *events.lock().unwrap());
    (events, sent)
}

/// The send stream's stage order is unchanged and only its last event, `Broadcasted`,
/// carries `result`: exactly the value the send resolves to. Covers a node that
/// accepts, one that refuses (`DoubleSpend`) and one that answers nothing.
#[test]
fn the_broadcasted_event_carries_the_send_result() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    for submit in [Some((true, "None")), Some((false, "DoubleSpend")), None] {
        let (events, sent) = send_via_mock_node_with_events(&funded, &runtime, submit);

        let stages: Vec<_> = events.iter().map(|e| format!("{:?}", e.stage)).collect();
        assert_eq!(
            stages,
            [
                "Initializing",
                "ValidatingInput",
                "ConnectingToNetwork",
                "ConstructingTransaction",
                "SigningKeyGeneration",
                "SigningTransaction",
                "Broadcasting",
                "Broadcasted",
            ],
            "{submit:?}"
        );
        let (last, earlier) = events.split_last().expect("events");
        assert!(
            earlier.iter().all(|e| e.result.is_none()),
            "only the last event carries a result ({submit:?})"
        );
        assert_eq!(last.details, "Transaction broadcast");
        let result = last
            .result
            .as_ref()
            .expect("Broadcasted carries the result");
        assert_eq!(result.transaction.id, sent.transaction.id);
        assert_eq!(result.accepted_by_node, sent.accepted_by_node);
        assert_eq!(result.node_reason, sent.node_reason);
        assert_eq!(result.note, sent.note);
        assert!(!result.transaction.id.is_empty());

        match submit {
            Some((true, _)) => {
                assert!(result.accepted_by_node);
                assert_eq!(result.node_reason, None);
            }
            Some((false, _)) => {
                assert!(!result.accepted_by_node);
                assert_eq!(
                    result.node_reason.as_deref(),
                    Some("Transaction not accepted: Double Spend")
                );
                assert!(result.note.is_some(), "the user is told not to resend");
            }
            None => {
                assert!(!result.accepted_by_node);
                assert!(result.node_reason.is_some());
            }
        }
    }
}

/// Record that the node refused the send's last rebroadcasts until it stalled, as
/// the scan's transaction monitor does.
fn stall(funded: &FundedWallet, transaction_id: &str) {
    let pool = minotari_wallet::db::init_db(funded.db.db_path.clone()).expect("pool");
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
/// inputs are released. The wallet's running scan is stopped first; another wallet's
/// scan keeps running.
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

    // Continuous scans of both wallets are running; abandon_send stops the funded
    // wallet's scan only.
    let continuous = |wallet: &str, base_url: String| ScanConfiguration {
        continuous: true,
        ..scan_config(wallet, base_url)
    };
    let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
    let (other_tx, mut other_rx) = tokio::sync::mpsc::unbounded_channel();
    let other_handle = create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle");
    let (scan, other_scan, _server) = runtime.block_on(async {
        let server = mock_chain_node().await;
        let scan = tokio::spawn(start_scan_with_handler(
            create_scan_handle(FUNDED.to_string()).expect("handle"),
            continuous(FUNDED, server.uri()),
            move |_| {
                let _ = seen_tx.send(());
                Ok(())
            },
        ));
        let other_scan = tokio::spawn(start_scan_with_handler(
            other_handle.clone(),
            continuous(fixture::WALLET_NAME, server.uri()),
            move |event| {
                let _ = other_tx.send(event);
                Ok(())
            },
        ));
        seen_rx.recv().await.expect("the funded scan is running");
        other_rx.recv().await.expect("the fixture scan is running");
        (scan, other_scan, server)
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
    assert!(!other_scan.is_finished(), "the other wallet keeps scanning");
    runtime
        .block_on(stop_scan(other_handle))
        .expect("stop_scan");
    runtime
        .block_on(other_scan)
        .unwrap()
        .expect("a stopped scan returns Ok");
    let mut last = None;
    while let Ok(event) = other_rx.try_recv() {
        last = Some(event);
    }
    assert!(
        matches!(last, Some(ScanEventDto::Cancelled)),
        "got {last:?}"
    );

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

/// An abandoned send that is mined after all: the scan that finds its change output
/// flags it in `abandoned_then_mined_ids`, and `get_abandon_warnings` reports it as
/// `MinedAfterAbandon`. The scan's event set is unchanged.
#[test]
fn an_abandoned_send_mined_after_all_is_reported() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    assert!(get_abandon_warnings(FUNDED.to_string())
        .expect("no warnings yet")
        .is_empty());
    assert_eq!(
        get_abandon_warnings("missing".to_string())
            .unwrap_err()
            .to_string(),
        "No accounts found for this wallet"
    );

    // Send through a node that answers nothing, keeping the signed transaction.
    let (sent, transaction) = runtime.block_on(async {
        let server = wiremock::MockServer::start().await;
        let details = funded_send(
            &funded,
            vec![SendRecipient {
                address: funded.recipient.clone(),
                amount: 100_000,
                payment_id: None,
            }],
            false,
            server.uri(),
        );
        let sent = send_transaction_with_handler(details, |_| {})
            .await
            .expect("send");
        let requests = server.received_requests().await.expect("recorded requests");
        let body: serde_json::Value =
            serde_json::from_slice(&requests.first().expect("a broadcast").body).expect("json");
        let transaction: tari_transaction_components::transaction_components::Transaction =
            serde_json::from_value(body["params"]["transaction"].clone()).expect("transaction");
        (sent, transaction)
    });
    let tx_id = sent.transaction.id;

    stall(&funded, &tx_id);
    runtime
        .block_on(abandon_send(
            FUNDED.to_string(),
            FUNDED_PASSPHRASE.to_string(),
            tx_id.clone(),
        ))
        .expect("abandon_send");
    assert!(
        get_abandon_warnings(FUNDED.to_string())
            .expect("abandoned, not mined")
            .is_empty(),
        "an abandon alone is no warning"
    );

    // The original is mined anyway, in the block after the wallet's tip.
    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    runtime
        .block_on(async {
            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::any())
                .respond_with(MockChain {
                    mined: Some(transaction),
                })
                .mount(&server)
                .await;
            let sink = events.clone();
            start_scan_with_handler(
                create_scan_handle(FUNDED.to_string()).expect("handle"),
                scan_config(FUNDED, server.uri()),
                move |event| {
                    sink.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
        })
        .expect("start_scan");

    let warnings = get_abandon_warnings(FUNDED.to_string()).expect("warnings");
    assert_eq!(warnings.len(), 1, "got {warnings:?}");
    assert_eq!(warnings[0].transaction_id, tx_id);
    assert_eq!(warnings[0].kind, AbandonWarningKind::MinedAfterAbandon);
    assert!(warnings[0].event_id > 0);

    let events = events.lock().unwrap();
    let flagged: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            ScanEventDto::TransactionsReady(e) => e.abandoned_then_mined_ids.clone(),
            ScanEventDto::TransactionsUpdated(e) => e.abandoned_then_mined_ids.clone(),
            _ => None,
        })
        .collect();
    assert!(
        events.iter().any(|event| matches!(
            event,
            ScanEventDto::TransactionsReady(e)
                if e.abandoned_then_mined_ids.as_deref() == Some(std::slice::from_ref(&tx_id))
                    && e.transactions.iter().any(|t| t.id == tx_id)
        )),
        "the TransactionsReady carrying the mined send flags it, got {events:?}"
    );
    assert!(
        flagged
            .iter()
            .all(|ids| ids == std::slice::from_ref(&tx_id)),
        "only the abandoned send is flagged, got {flagged:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(ScanEventDto::Completed {
                final_height,
                ..
            }) if *final_height == MOCK_TIP + 1
        ),
        "got {events:?}"
    );
    assert_eq!(events.iter().filter(|e| is_terminal(e)).count(), 1);
}

/// A view-only wallet sending with its stored keys is refused before anything is
/// locked.
#[test]
fn view_only_wallet_cannot_send_with_stored_keys() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let funded = with_funded_wallet();

    let details = MultiSendTransactionDetails {
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
        .block_on(send_transaction_with_handler(details, |_| {}))
        .expect_err("a view-only wallet cannot send");
    assert_eq!(
        err.to_string(),
        "Signing Error: View-only wallets cannot send"
    );
    let balance = get_balance(fixture::WALLET_NAME.to_string()).expect("balance");
    assert_eq!(balance.locked, 0);
    assert_eq!(balance.total, fixture::GOLDEN_TOTAL);
}

// ---------------------------------------------------------------------------
// start_scan against a mock chain.
// ---------------------------------------------------------------------------

/// A mock base node whose chain runs to `MOCK_TIP` with no wallet outputs. Its tip
/// matches the funded wallet's scanned tip, so a plain scan of that wallet resumes
/// with nothing to do and only a rollback removes its outputs.
///
/// With `mined` set, the chain has one more block, at `MOCK_TIP + 1`, holding that
/// transaction (its outputs, and its inputs as spent output hashes).
#[derive(Default)]
struct MockChain {
    mined: Option<tari_transaction_components::transaction_components::Transaction>,
}

impl MockChain {
    fn tip(&self) -> u64 {
        MOCK_TIP + u64::from(self.mined.is_some())
    }

    /// The outputs and spent output hashes of the block at `height`.
    fn block_contents(
        &self,
        height: u64,
    ) -> (
        Vec<tari_transaction_components::transaction_components::TransactionOutput>,
        Vec<Vec<u8>>,
    ) {
        match &self.mined {
            Some(tx) if height == MOCK_TIP + 1 => (
                tx.body().outputs().to_vec(),
                tx.body()
                    .inputs()
                    .iter()
                    .map(|input| input.output_hash().to_vec())
                    .collect(),
            ),
            _ => (Vec::new(), Vec::new()),
        }
    }
}

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
            BlockUtxoInfo, GetUtxosByBlockResponse, MinimalUtxoSyncInfo,
            SyncUtxosByBlockResponseV0, SyncUtxosByBlockResponseV1,
        };
        use tari_utilities::hex::from_hex;
        use tari_utilities::ByteArray;
        use wiremock::ResponseTemplate;

        let tip = self.tip();
        match request.url.path() {
            "/get_tip_info" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "metadata": {
                    "best_block_height": tip,
                    "best_block_hash": mock_block_hash(tip),
                    "accumulated_difficulty": "0",
                    "pruned_height": 0,
                    "timestamp": 0,
                }
            })),
            "/get_height_at_time" => ResponseTemplate::new(200).set_body_json(0u64),
            "/get_header_by_height" => {
                let height: u64 = mock_query(request, "height").parse().expect("height");
                if height > tip {
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
                let last = (start + limit - 1).min(tip);
                let blocks = (start..=last)
                    .map(|height| {
                        let (outputs, inputs) = self.block_contents(height);
                        BlockUtxoInfo {
                            header_hash: mock_block_hash(height),
                            height,
                            outputs: outputs
                                .iter()
                                .map(|output| MinimalUtxoSyncInfo {
                                    output_hash: output.hash().to_vec(),
                                    commitment: output.commitment.as_bytes().to_vec(),
                                    encrypted_data: output.encrypted_data.to_byte_vec(),
                                    sender_offset_public_key: output
                                        .sender_offset_public_key
                                        .as_bytes()
                                        .to_vec(),
                                })
                                .collect(),
                            inputs,
                            mined_timestamp: 1_700_000_000,
                        }
                    })
                    .collect();
                let next_header_to_scan = if last < tip {
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
            "/get_utxos_by_block" => {
                let hash = from_hex(&mock_query(request, "header_hash")).expect("hex");
                let height = u64::from_be_bytes(hash[0..8].try_into().expect("8 bytes"));
                let (outputs, _) = self.block_contents(height);
                ResponseTemplate::new(200).set_body_json(GetUtxosByBlockResponse {
                    header_hash: hash,
                    height,
                    outputs,
                    mined_timestamp: 1_700_000_000,
                })
            }
            _ => ResponseTemplate::new(404),
        }
    }
}

#[test]
fn start_scan_scans_to_the_tip() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();
    db.unlock(fixture::WALLET_NAME, "").expect("unlock");

    let events = std::sync::Arc::new(Mutex::new(Vec::new()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime
        .block_on(async {
            let server = mock_chain_node().await;
            let sink = events.clone();
            start_scan_with_handler(
                create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle"),
                scan_config(fixture::WALLET_NAME, server.uri()),
                move |event| {
                    sink.lock().unwrap().push(event);
                    Ok(())
                },
            )
            .await
        })
        .expect("start_scan");

    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ScanEventDto::Progress {
                tip_height: MOCK_TIP,
                ..
            }
        )),
        "progress carries the tip, got {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(ScanEventDto::Completed {
                final_height: MOCK_TIP,
                ..
            })
        ),
        "got {events:?}"
    );
    let terminals = events.iter().filter(|e| is_terminal(e)).count();
    assert_eq!(terminals, 1, "exactly one terminal event");
}

fn is_terminal(event: &ScanEventDto) -> bool {
    matches!(
        event,
        ScanEventDto::Completed { .. }
            | ScanEventDto::Cancelled
            | ScanEventDto::Superseded
            | ScanEventDto::BudgetExhausted { .. }
            | ScanEventDto::Error(_)
    )
}

/// A running continuous Background scan of the fixture wallet is replaced by a second
/// scan with `second` priority: the first stream ends with exactly one terminal event,
/// `Superseded`, and the second scan runs to the tip.
fn assert_superseded_by(second: ScanPriority) {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let db = with_fixture_db();
    db.unlock(fixture::WALLET_NAME, "").expect("unlock");

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (first_events, second_events) = runtime.block_on(async {
        let server = mock_chain_node().await;
        let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
        let first = tokio::spawn(start_scan_with_handler(
            create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle"),
            ScanConfiguration {
                continuous: true,
                priority: ScanPriority::Background,
                ..scan_config(fixture::WALLET_NAME, server.uri())
            },
            move |event| {
                let _ = first_tx.send(event);
                Ok(())
            },
        ));
        let mut first_events = vec![first_rx.recv().await.expect("the first scan is running")];

        let second_events = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = second_events.clone();
        start_scan_with_handler(
            create_scan_handle(fixture::WALLET_NAME.to_string()).expect("handle"),
            ScanConfiguration {
                priority: second,
                ..scan_config(fixture::WALLET_NAME, server.uri())
            },
            move |event| {
                sink.lock().unwrap().push(event);
                Ok(())
            },
        )
        .await
        .expect("the second scan");

        first.await.unwrap().expect("a superseded scan resolves Ok");
        // The first scan's callback is dropped once it returns, closing the channel.
        while let Some(event) = first_rx.recv().await {
            first_events.push(event);
        }
        let second_events = std::mem::take(&mut *second_events.lock().unwrap());
        (first_events, second_events)
    });

    let terminals: Vec<_> = first_events.iter().filter(|e| is_terminal(e)).collect();
    assert!(
        matches!(terminals.as_slice(), [ScanEventDto::Superseded]),
        "the replaced scan ends with exactly one Superseded, got {first_events:?}"
    );
    assert!(
        matches!(first_events.last(), Some(ScanEventDto::Superseded)),
        "Superseded is its last event, got {first_events:?}"
    );
    assert_eq!(second_events.iter().filter(|e| is_terminal(e)).count(), 1);
    assert!(
        matches!(
            second_events.last(),
            Some(ScanEventDto::Completed {
                final_height: MOCK_TIP,
                ..
            })
        ),
        "got {second_events:?}"
    );
}

#[test]
fn a_foreground_scan_supersedes_a_running_background_scan() {
    assert_superseded_by(ScanPriority::Foreground);
}

#[test]
fn a_same_priority_scan_supersedes_the_running_one() {
    assert_superseded_by(ScanPriority::Background);
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

use minotari_wallet_ffi::api::balance::AccountBalanceDto;
use minotari_wallet_ffi::api::transactions::DisplayedTransactionDto;
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
