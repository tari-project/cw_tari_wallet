//! Exercise the real upstream monitor through the wrapper's scan entry point.
//! No separate confirmation refresh, live node, or real wallet is involved.

use minotari_wallet::db;
use rust_lib_flutter_rust_wallet::api::{db as bridge_db, scanner, transactions};
use std::sync::{Arc, Mutex};
use tari_common_types::types::FixedHash;
use verify::fixture;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn block_hash(height: u64) -> Vec<u8> {
    let mut hash = vec![0x5a; 32];
    hash[..8].copy_from_slice(&height.to_be_bytes());
    hash
}

struct EmptyChain {
    tip: u64,
}

impl Respond for EmptyChain {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        match request.url.path() {
            "/get_tip_info" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "metadata": {
                    "best_block_height": self.tip,
                    "best_block_hash": block_hash(self.tip),
                    "accumulated_difficulty": "0", "pruned_height": 0, "timestamp": 0,
                }
            })),
            "/get_header_by_height" => {
                let height: u64 = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "height")
                    .unwrap()
                    .1
                    .parse()
                    .unwrap();
                if height > self.tip {
                    return ResponseTemplate::new(404);
                }
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "height": height, "hash": block_hash(height), "timestamp": 1_700_000_000u64,
                }))
            }
            // This wallet already scanned the tip. An empty response ends a
            // Full pass and must still run the monitor (the #149 regression).
            "/sync_utxos_by_block" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "blocks": [], "has_next_page": false, "next_header_to_scan": [],
            })),
            _ => ResponseTemplate::new(404),
        }
    }
}

struct Disconnect;
impl Drop for Disconnect {
    fn drop(&mut self) {
        let _ = bridge_db::disconnect_database();
    }
}

#[tokio::test]
async fn scan_updates_history_without_the_old_confirmation_workaround() {
    let _serial = SERIAL.lock().await;
    let (_dir, db_path) = fixture::materialize_fixture_db().unwrap();
    let pool = db::init_db(db_path.clone()).unwrap();
    let tip = fixture::TX_BLOCK_HEIGHT + 3;
    {
        let conn = pool.get().unwrap();
        let account = db::get_accounts(&conn, Some(fixture::WALLET_NAME))
            .unwrap()
            .remove(0);
        let mut tx = db::get_displayed_transactions_paginated(&conn, account.id, 100, 0)
            .unwrap()
            .remove(0);
        tx.status = minotari_wallet::transactions::TransactionDisplayStatus::Unconfirmed;
        tx.blockchain.confirmations = 0;
        tx.blockchain.block_hash =
            FixedHash::new(block_hash(fixture::TX_BLOCK_HEIGHT).try_into().unwrap());
        db::update_displayed_transaction_confirmations(&conn, &tx).unwrap();
        tx.id = 43u64.into();
        tx.direction = minotari_wallet::transactions::TransactionDirection::Outgoing;
        tx.blockchain.block_height = 0;
        tx.blockchain.block_hash = FixedHash::zero();
        tx.status = minotari_wallet::transactions::TransactionDisplayStatus::Pending;
        db::insert_displayed_transaction(&conn, &tx).unwrap();
        db::insert_scanned_tip_block(&conn, account.id, tip as i64, &block_hash(tip)).unwrap();
    }
    drop(pool);
    bridge_db::initialize_database(db_path.to_string_lossy().into_owned()).unwrap();
    let _disconnect = Disconnect;
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(EmptyChain { tip })
        .mount(&server)
        .await;

    for pass in 0..2 {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        scanner::start_scan_with_handler(
            scanner::ScanConfiguration {
                wallet_name: fixture::WALLET_NAME.into(),
                passphrase: String::new(),
                base_url: server.uri(),
                batch_size: 100,
                continuous: false,
                poll_interval_seconds: 20,
                required_confirmations: 3,
            },
            move |event| {
                captured.lock().unwrap().push(event);
                Ok(())
            },
        )
        .await
        .unwrap();
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            scanner::ScanEventDto::Status(scanner::ScanStatusDto::Completed { .. })
        )));
        if pass == 0 {
            assert!(events.iter().any(|event| matches!(event,
                scanner::ScanEventDto::TransactionsUpdated(update) if update.updated_transactions.iter().any(|tx|
                    tx.id == fixture::TX_ID.to_string() && tx.blockchain.confirmations == 3
                    && matches!(tx.status, transactions::DisplayedTransactionStatus::Confirmed)))));
        }
        drop(events);
        let changes =
            transactions::get_changed_transactions(fixture::WALLET_NAME.into(), None).unwrap();
        assert_eq!(changes.len(), if pass == 0 { 1 } else { 0 });
        assert!(
            transactions::get_changed_transactions(fixture::WALLET_NAME.into(), None)
                .unwrap()
                .is_empty()
        );
    }
    let history = transactions::get_transactions(fixture::WALLET_NAME.into(), 100, 0).unwrap();
    let mined = history
        .iter()
        .find(|tx| tx.id == fixture::TX_ID.to_string())
        .unwrap();
    assert_eq!(mined.blockchain.confirmations, 3);
    assert!(matches!(
        mined.status,
        transactions::DisplayedTransactionStatus::Confirmed
    ));
    let unmined = history.iter().find(|tx| tx.id == "43").unwrap();
    assert_eq!(unmined.blockchain.confirmations, 0);
    assert!(matches!(
        unmined.status,
        transactions::DisplayedTransactionStatus::Pending
    ));
}

/// Force an early WAL read from the real forwarder callback while a synthetic
/// native write is uncommitted. There is no second block/event to repair the UI.
#[tokio::test]
async fn scan_delivers_committed_outgoing_after_an_early_read() {
    assert_committed_delivery(false).await;
}

#[tokio::test]
async fn scan_delivers_new_incoming_after_an_early_empty_read() {
    assert_committed_delivery(true).await;
}

async fn assert_committed_delivery(incoming: bool) {
    use minotari_wallet::transactions::{TransactionDirection, TransactionDisplayStatus};
    use std::sync::atomic::{AtomicBool, Ordering};
    let _serial = SERIAL.lock().await;
    {
        let (_dir, db_path) = fixture::materialize_fixture_db().unwrap();
        let pool = db::init_db(db_path.clone()).unwrap();
        let tip = fixture::TX_BLOCK_HEIGHT + 3;
        {
            let conn = pool.get().unwrap();
            let account = db::get_accounts(&conn, Some(fixture::WALLET_NAME))
                .unwrap()
                .remove(0);
            let mut tx = db::get_displayed_transactions_paginated(&conn, account.id, 100, 0)
                .unwrap()
                .remove(0);
            tx.status = TransactionDisplayStatus::Unconfirmed;
            tx.blockchain.confirmations = 0;
            db::update_displayed_transaction_confirmations(&conn, &tx).unwrap();
            db::insert_scanned_tip_block(&conn, account.id, tip as i64, &block_hash(tip)).unwrap();
        }
        drop(pool);
        bridge_db::initialize_database(db_path.to_string_lossy().into_owned()).unwrap();
        let _disconnect = Disconnect;
        let server = MockServer::start().await;
        Mock::given(wiremock::matchers::any())
            .respond_with(EmptyChain { tip })
            .mount(&server)
            .await;
        let probed = Arc::new(AtomicBool::new(false));
        let captured = probed.clone();
        scanner::start_scan_with_handler(
            scanner::ScanConfiguration {
                wallet_name: fixture::WALLET_NAME.into(),
                passphrase: String::new(),
                base_url: server.uri(),
                batch_size: 100,
                continuous: false,
                poll_interval_seconds: 20,
                required_confirmations: 3,
            },
            move |event| {
                if captured.load(Ordering::SeqCst) {
                    // Drain every later notification too: a second upstream event
                    // must not accidentally repair an otherwise lost update.
                    transactions::get_changed_transactions(fixture::WALLET_NAME.into(), None)?;
                    return Ok(());
                }
                if !matches!(event, scanner::ScanEventDto::TransactionsUpdated(_)) {
                    return Ok(());
                }
                captured.store(true, Ordering::SeqCst);
                let writer = rusqlite::Connection::open(&db_path)?;
                let json: String = writer.query_row(
                    "SELECT transaction_json FROM displayed_transactions WHERE id='42'",
                    [],
                    |row| row.get(0),
                )?;
                let mut tx: minotari_wallet::DisplayedTransaction = serde_json::from_str(&json)?;
                tx.direction = if incoming {
                    TransactionDirection::Incoming
                } else {
                    TransactionDirection::Outgoing
                };
                tx.status = TransactionDisplayStatus::Pending;
                tx.blockchain.block_height = 0;
                tx.blockchain.confirmations = 0;
                if incoming {
                    // No committed row yet, as with a newly discovered receipt.
                    writer.execute("DELETE FROM displayed_transactions WHERE id='42'", [])?;
                } else {
                    writer.execute(
                        "UPDATE displayed_transactions SET transaction_json=?1 WHERE id='42'",
                        [serde_json::to_string(&tx)?],
                    )?;
                }
                writer.execute_batch("BEGIN IMMEDIATE")?;
                tx.blockchain.block_height = fixture::TX_BLOCK_HEIGHT;
                tx.status = TransactionDisplayStatus::Unconfirmed;
                if incoming {
                    db::insert_displayed_transaction(&writer, &tx)?;
                } else {
                    writer.execute(
                        "UPDATE displayed_transactions SET transaction_json=?1 WHERE id='42'",
                        [serde_json::to_string(&tx)?],
                    )?;
                }
                // This consumes/acknowledges the dirty ID recorded by the forwarder.
                let early =
                    transactions::get_changed_transactions(fixture::WALLET_NAME.into(), None)?;
                if incoming {
                    assert!(early.is_empty());
                } else {
                    assert_eq!(early[0].blockchain.block_height, 0);
                }
                writer.execute_batch("COMMIT")?;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(probed.load(Ordering::SeqCst));
        let committed =
            transactions::get_changed_transactions(fixture::WALLET_NAME.into(), None).unwrap();
        assert_eq!(
            committed.len(),
            1,
            "early acknowledgement must not lose the committed row"
        );
        assert_eq!(
            committed[0].blockchain.block_height,
            fixture::TX_BLOCK_HEIGHT
        );
        assert_eq!(committed[0].blockchain.confirmations, 0);
        assert!(matches!(
            committed[0].status,
            transactions::DisplayedTransactionStatus::Unconfirmed
        ));
        assert_eq!(
            matches!(
                committed[0].direction,
                transactions::DisplayedTransactionDirection::Incoming
            ),
            incoming
        );
        assert!(
            transactions::get_changed_transactions(fixture::WALLET_NAME.into(), None)
                .unwrap()
                .is_empty()
        );
    }
}
