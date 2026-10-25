//! Deletion must affect one account only and roll back every table on failure.
//! These tests use disposable synthetic fixtures, never an application wallet.
use minotari_wallet::db;
use rust_lib_flutter_rust_wallet::api::{
    address::get_address,
    balance::get_balance,
    db::{disconnect_database, initialize_database},
    network::TariNetwork,
    transactions::get_transactions,
    wallet::{delete_wallet, import_view_only_wallet, list_wallets},
};
use std::{path::PathBuf, sync::Mutex};
use verify::fixture;

static SERIAL: Mutex<()> = Mutex::new(());
const KEEPER: &str = "keeper";

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = disconnect_database();
    }
}
fn setup() -> Fixture {
    let (dir, path) = fixture::materialize_fixture_db().unwrap();
    initialize_database(path.to_string_lossy().into_owned()).unwrap();
    import_view_only_wallet(
        KEEPER.into(),
        "0800000000000000000000000000000000000000000000000000000000000000".into(),
        fixture::spend_public_key_hex(),
        fixture::BIRTHDAY,
        String::new(),
        Some(TariNetwork::Esmeralda),
    )
    .unwrap();
    Fixture { _dir: dir, path }
}
fn address(name: &str) -> String {
    get_address(
        name.into(),
        Some(String::new()),
        Some(TariNetwork::Esmeralda),
    )
    .unwrap()
}
fn row_counts(path: &std::path::Path) -> Vec<(String, i64)> {
    let pool = db::init_db(path.to_owned()).unwrap();
    let conn = pool.get().unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|name| {
            let count = conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{name}\""), [], |row| {
                    row.get(0)
                })
                .unwrap();
            (name, count)
        })
        .collect()
}

#[test]
fn deleting_one_account_preserves_another_wallet_and_its_history() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fixture = setup();
    let before_address = address(fixture::WALLET_NAME);
    delete_wallet(KEEPER.into()).unwrap();
    assert_eq!(list_wallets().unwrap(), vec![fixture::WALLET_NAME]);
    assert_eq!(address(fixture::WALLET_NAME), before_address);
    assert_eq!(
        get_balance(fixture::WALLET_NAME.into()).unwrap().total,
        fixture::GOLDEN_TOTAL
    );
    assert_eq!(
        get_transactions(fixture::WALLET_NAME.into(), 100, 0)
            .unwrap()
            .len(),
        1
    );
    assert!(fixture.path.exists(), "the shared database must survive");
}

#[test]
fn deletion_removes_history_and_allows_reuse_of_the_name() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fixture = setup();
    let keeper_address = address(KEEPER);
    delete_wallet(fixture::WALLET_NAME.into()).unwrap();
    assert_eq!(list_wallets().unwrap(), vec![KEEPER]);
    assert_eq!(address(KEEPER), keeper_address);
    let counts = row_counts(&fixture.path);
    for table in [
        "balance_changes",
        "displayed_transactions",
        "inputs",
        "outputs",
        "events",
        "pending_transactions",
        "completed_transactions",
        "scanned_tip_blocks",
        "burn_proofs",
        "payref_history",
        "webhook_queue",
    ] {
        assert_eq!(
            counts.iter().find(|(name, _)| name == table).unwrap().1,
            0,
            "{table}"
        );
    }
    // Same fixture keys are valid again once the original account is removed.
    import_view_only_wallet(
        fixture::WALLET_NAME.into(),
        fixture::VIEW_PRIVATE_KEY_HEX.into(),
        fixture::spend_public_key_hex(),
        fixture::BIRTHDAY,
        String::new(),
        Some(TariNetwork::Esmeralda),
    )
    .unwrap();
    assert_eq!(get_balance(fixture::WALLET_NAME.into()).unwrap().total, 0);
    assert!(get_transactions(fixture::WALLET_NAME.into(), 100, 0)
        .unwrap()
        .is_empty());
}

#[test]
fn a_late_delete_error_rolls_back_account_history_and_balance() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fixture = setup();
    let before = row_counts(&fixture.path);
    let pool = db::init_db(fixture.path.clone()).unwrap();
    let conn = pool.get().unwrap();
    conn.execute_batch("CREATE TRIGGER fail_account_delete BEFORE DELETE ON accounts BEGIN SELECT RAISE(ABORT, 'injected deletion failure'); END;").unwrap();
    assert!(delete_wallet(fixture::WALLET_NAME.into()).is_err());
    assert_eq!(
        row_counts(&fixture.path),
        before,
        "all earlier child deletes must roll back"
    );
    assert_eq!(
        get_balance(fixture::WALLET_NAME.into()).unwrap().total,
        fixture::GOLDEN_TOTAL
    );
    assert_eq!(
        get_transactions(fixture::WALLET_NAME.into(), 100, 0)
            .unwrap()
            .len(),
        1
    );
    conn.execute_batch("DROP TRIGGER fail_account_delete")
        .unwrap();
    delete_wallet(fixture::WALLET_NAME.into()).unwrap();
    delete_wallet(KEEPER.into()).unwrap();
    assert!(list_wallets().unwrap().is_empty());
    assert!(
        fixture.path.exists(),
        "even the last account deletion preserves wallet.db"
    );
}

#[test]
fn missing_account_is_an_error_without_changes_to_other_accounts() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fixture = setup();
    let before = row_counts(&fixture.path);
    assert!(delete_wallet("absent".into()).is_err());
    assert_eq!(row_counts(&fixture.path), before);
}
