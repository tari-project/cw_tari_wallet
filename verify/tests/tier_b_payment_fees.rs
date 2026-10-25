//! Concrete XTM payment estimates against synthetic UTXOs and a local node.
use minotari_wallet::{db, transactions::fee_estimator::estimated_output_size_for_payment_id};
use rust_lib_flutter_rust_wallet::api::{
    db::{disconnect_database, initialize_database},
    fee::{estimate_payment_fee, FeePriority},
    send_transaction::{send_transaction_with_fee_handler, SendTransactionDetails},
};
use tari_common_types::types::FixedHash;
use tari_transaction_components::{transaction_components::WalletOutput, MicroMinotari};
use verify::fixture;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
struct Disconnect;
impl Drop for Disconnect {
    fn drop(&mut self) {
        let _ = disconnect_database();
    }
}
fn add_output(conn: &rusqlite::Connection, account: i64, hash: u8, value: u64) {
    let output = WalletOutput::new_from_parts(
        Default::default(),
        MicroMinotari(value),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        0,
        Default::default(),
        Default::default(),
        MicroMinotari(0),
        None,
        Default::default(),
        FixedHash::from([hash; 32]),
        Default::default(),
    );
    conn.execute("INSERT INTO outputs (account_id, tx_id, output_hash, mined_in_block_height, mined_in_block_hash, value, mined_timestamp, wallet_output_json, status, confirmed_height, confirmed_hash, is_burn, maturity) VALUES (?1, ?2, ?3, 100, ?3, ?4, ?5, ?6, 'UNSPENT', 100, ?3, 0, 0)",
        rusqlite::params![account, i64::from(hash), vec![hash;32], i64::try_from(value).unwrap(), chrono::Utc::now(), serde_json::to_string(&output).unwrap()]).unwrap();
}
async fn node(body: serde_json::Value) -> MockServer {
    let node = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/get_mempool_fee_per_gram_stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&node)
        .await;
    node
}
fn stats() -> serde_json::Value {
    serde_json::json!({"stats":[
        {"order":0,"min_fee_per_gram":1,"avg_fee_per_gram":10,"max_fee_per_gram":20},
        {"order":1,"min_fee_per_gram":1,"avg_fee_per_gram":5,"max_fee_per_gram":10},
        {"order":2,"min_fee_per_gram":1,"avg_fee_per_gram":2,"max_fee_per_gram":5}
    ]})
}
#[tokio::test]
async fn amount_priority_and_memo_use_native_selection_without_reserving_funds() {
    let _serial = SERIAL.lock().await;
    let (_dir, db_path) = fixture::materialize_fixture_db().unwrap();
    let pool = db::init_db(db_path.clone()).unwrap();
    let conn = pool.get().unwrap();
    let account = db::get_accounts(&conn, Some(fixture::WALLET_NAME))
        .unwrap()
        .remove(0);
    db::insert_scanned_tip_block(&conn, account.id, 200, &[0; 32]).unwrap();
    add_output(&conn, account.id, 1, 1_000_000);
    add_output(&conn, account.id, 2, 2_000_000);
    initialize_database(db_path.to_string_lossy().into()).unwrap();
    let _disconnect = Disconnect;
    let node = node(stats()).await;
    let quote = |amount, priority, memo| {
        estimate_payment_fee(
            amount,
            priority,
            node.uri(),
            fixture::WALLET_NAME.into(),
            memo,
        )
    };
    let small = quote(100_000, FeePriority::Medium, None).await.unwrap();
    let large = quote(2_500_000, FeePriority::Medium, None).await.unwrap();
    assert_eq!(small.input_count, 1);
    assert_eq!(large.input_count, 2);
    assert!(large.estimated_fee > small.estimated_fee);
    assert_eq!(large.total_amount_required, 2_500_000 + large.estimated_fee);
    let slow = quote(100_000, FeePriority::Slow, None).await.unwrap();
    let fast = quote(100_000, FeePriority::Fast, None).await.unwrap();
    assert_eq!(
        (slow.fee_per_gram, small.fee_per_gram, fast.fee_per_gram),
        (1, 5, 10)
    );
    assert!(slow.estimated_fee < small.estimated_fee && small.estimated_fee < fast.estimated_fee);
    let long_memo = quote(100_000, FeePriority::Medium, Some("я".repeat(50)))
        .await
        .unwrap();
    assert!(long_memo.estimated_fee > small.estimated_fee);
    let selection =
        minotari_wallet::transactions::input_selector::InputSelector::new(account.id, 3)
            .fetch_unspent_outputs(
                &conn,
                MicroMinotari(100_000),
                1,
                MicroMinotari(5),
                Some(estimated_output_size_for_payment_id(100).unwrap()),
            )
            .unwrap();
    assert_eq!(long_memo.estimated_fee, selection.fee().0);
    let locked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM outputs WHERE status != 'UNSPENT'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(locked, 0);
    let pending: i64 = conn
        .query_row("SELECT COUNT(*) FROM pending_transactions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(pending, 0);
    assert!(quote(0, FeePriority::Medium, None).await.is_err());
    assert!(quote(3_000_000, FeePriority::Slow, None).await.is_err());
}
#[tokio::test]
async fn slow_payment_is_not_rejected_for_lacking_fast_fee_funds() {
    let _serial = SERIAL.lock().await;
    let (_dir, db_path) = fixture::materialize_fixture_db().unwrap();
    let pool = db::init_db(db_path.clone()).unwrap();
    let conn = pool.get().unwrap();
    let account = db::get_accounts(&conn, Some(fixture::WALLET_NAME))
        .unwrap()
        .remove(0);
    db::insert_scanned_tip_block(&conn, account.id, 200, &[0; 32]).unwrap();
    add_output(&conn, account.id, 1, 10_000);
    initialize_database(db_path.to_string_lossy().into()).unwrap();
    let _disconnect = Disconnect;
    let node = node(stats()).await;
    let slow = estimate_payment_fee(
        9_500,
        FeePriority::Slow,
        node.uri(),
        fixture::WALLET_NAME.into(),
        None,
    )
    .await
    .unwrap();
    assert!(slow.total_amount_required < 10_000);
    assert!(estimate_payment_fee(
        9_500,
        FeePriority::Fast,
        node.uri(),
        fixture::WALLET_NAME.into(),
        None
    )
    .await
    .is_err());
}
#[tokio::test]
async fn fallback_and_zero_rates_never_quote_zero_fee() {
    let _serial = SERIAL.lock().await;
    let (_dir, db_path) = fixture::materialize_fixture_db().unwrap();
    let pool = db::init_db(db_path.clone()).unwrap();
    let conn = pool.get().unwrap();
    let account = db::get_accounts(&conn, Some(fixture::WALLET_NAME))
        .unwrap()
        .remove(0);
    db::insert_scanned_tip_block(&conn, account.id, 200, &[0; 32]).unwrap();
    add_output(&conn, account.id, 1, 1_000_000);
    initialize_database(db_path.to_string_lossy().into()).unwrap();
    let _disconnect = Disconnect;
    for body in [
        serde_json::json!({"stats":[]}),
        serde_json::json!({"invalid":"stats response"}),
        serde_json::json!({"stats":[{"order":0,"min_fee_per_gram":0,"avg_fee_per_gram":0,"max_fee_per_gram":0}]}),
    ] {
        let zero = body["stats"]
            .as_array()
            .is_some_and(|stats| stats.len() == 1);
        let node = node(body).await;
        for (priority, fallback) in [
            (FeePriority::Slow, 1),
            (FeePriority::Medium, 5),
            (FeePriority::Fast, 10),
        ] {
            let quote = estimate_payment_fee(
                100_000,
                priority,
                node.uri(),
                fixture::WALLET_NAME.into(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(quote.fee_per_gram, if zero { 1 } else { fallback });
            assert!(quote.estimated_fee > 0);
        }
    }
}
#[tokio::test]
async fn invalid_send_fee_is_rejected_before_database_access_or_events() {
    let _serial = SERIAL.lock().await;
    disconnect_database().unwrap();
    for limits in [(0, 1000), (5, 0)] {
        let details = SendTransactionDetails {
            seed_words: vec![],
            passphrase: None,
            network: None,
            base_url: None,
            wallet_name: "synthetic".into(),
            recipient_address: "unused".into(),
            amount: 1,
            payment_id: None,
            confirmation_window: None,
        };
        let error = send_transaction_with_fee_handler(details, Some(limits), |_| {
            panic!("must not start sending")
        })
        .await
        .unwrap_err();
        assert!(!error.to_string().contains("not initialized"));
    }
}

#[tokio::test]
async fn explicit_rate_reaches_native_reservation_and_failure_releases_it() {
    use rust_lib_flutter_rust_wallet::api::{network::TariNetwork, wallet::create_wallet};
    let _serial = SERIAL.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sender.db");
    initialize_database(path.to_string_lossy().into()).unwrap();
    let _disconnect = Disconnect;
    let created = create_wallet(
        "sender".into(),
        Some(TariNetwork::Esmeralda),
        "test-only".into(),
    )
    .unwrap();
    let pool = db::init_db(path).unwrap();
    let conn = pool.get().unwrap();
    let account = db::get_accounts(&conn, Some("sender")).unwrap().remove(0);
    db::insert_scanned_tip_block(&conn, account.id, 200, &[0; 32]).unwrap();
    // Synthetic input can be selected/reserved but cannot be signed for broadcast.
    add_output(&conn, account.id, 1, 1_000_000);
    let selected = minotari_wallet::transactions::input_selector::InputSelector::new(account.id, 3)
        .fetch_unspent_outputs(
            &conn,
            MicroMinotari(100_000),
            1,
            MicroMinotari(17),
            Some(estimated_output_size_for_payment_id(0).unwrap()),
        )
        .unwrap();
    let details = SendTransactionDetails {
        seed_words: created.seed_words.unwrap().words.clone(),
        passphrase: Some("test-only".into()),
        network: Some(TariNetwork::Esmeralda),
        base_url: Some("http://127.0.0.1:1".into()),
        wallet_name: "sender".into(),
        recipient_address: created.tari_address,
        amount: 100_000,
        payment_id: None,
        confirmation_window: None,
    };
    let stages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = stages.clone();
    let result =
        send_transaction_with_fee_handler(details, Some((17, selected.fee().0)), move |e| {
            seen.lock().unwrap().push(e.stage);
        })
        .await;
    assert!(
        result.is_err(),
        "the synthetic input must never be broadcast"
    );
    assert!(!stages.lock().unwrap().iter().any(|s| matches!(
        s,
        rust_lib_flutter_rust_wallet::api::send_transaction::TransactionStage::Broadcasting
    )));
    let reserved_fee: i64 = conn
        .query_row(
            "SELECT fee_with_change FROM pending_transactions WHERE account_id=?1",
            [account.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        u64::try_from(reserved_fee).unwrap(),
        selected.fee().0,
        "send must use confirmed rate 17, not default 5"
    );
    let locked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM outputs WHERE status='LOCKED'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        locked, 0,
        "failed construction/signing must release the reservation"
    );
}
