use crate::api::{db::get_db_connection, error::WalletError, utils::format_micro_tari};
use crate::domain::history::{kernel_fee, normalize_local_send, recovered_send_fee};
use crate::domain::history_changes::{HistoryChanges, ScanHistoryChanges};
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::{
    db::{
        get_displayed_transactions_needing_confirmation_update,
        get_displayed_transactions_paginated, get_latest_scanned_tip_block_by_account,
        update_displayed_transaction_confirmations,
    },
    get_accounts,
    utils::timestamp::format_timestamp,
};
use once_cell::sync::Lazy;
use r2d2_sqlite::rusqlite::{Connection, OptionalExtension};
use std::sync::Mutex;
use tari_common_types::{payment_reference::generate_payment_reference, types::FixedHash};
use tari_transaction_components::transaction_components::Transaction;
use tari_utilities::ByteArray;

static HISTORY_CHANGES: Lazy<Mutex<HistoryChanges>> =
    Lazy::new(|| Mutex::new(HistoryChanges::default()));

pub(crate) fn record_transaction_changes(account: i64, ids: impl IntoIterator<Item = String>) {
    HISTORY_CHANGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .record(account, ids);
}

pub(crate) fn begin_scan_history_changes() -> ScanHistoryChanges {
    HISTORY_CHANGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .begin_scan()
}

pub(crate) fn record_scan_transaction_changes(
    scan: &mut ScanHistoryChanges,
    account: i64,
    ids: impl IntoIterator<Item = String>,
) {
    HISTORY_CHANGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .record_scan(scan, account, ids);
}

pub(crate) fn finish_scan_history_changes(scan: ScanHistoryChanges) {
    HISTORY_CHANGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .finish_scan(scan);
}

pub(crate) fn reset_transaction_changes() {
    HISTORY_CHANGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
}
/// Fee charged on a transaction: `amount` in **microTari** plus a pre-formatted
/// `amount_display` string for the UI.
#[frb]
#[derive(Debug, Clone)]
pub struct FeeInfoDto {
    pub amount: u64,
    pub amount_display: String,
}

impl From<minotari_wallet::transactions::FeeInfo> for FeeInfoDto {
    fn from(f: minotari_wallet::transactions::FeeInfo) -> Self {
        Self {
            amount: f.amount.0,
            amount_display: format_micro_tari(f.amount.0),
        }
    }
}

/// Where a transaction sits on chain: `block_height`, a formatted `timestamp`
/// string, and the number of `confirmations` (blocks) on top of it.
#[frb]
#[derive(Debug, Clone)]
pub struct BlockchainInfoDto {
    pub block_height: u64,
    pub timestamp: String,
    pub confirmations: u64,
}

impl From<minotari_wallet::transactions::BlockchainInfo> for BlockchainInfoDto {
    fn from(i: minotari_wallet::transactions::BlockchainInfo) -> Self {
        Self {
            block_height: i.block_height,
            timestamp: format_timestamp(i.timestamp),
            confirmations: i.confirmations,
        }
    }
}

/// The other party to a transaction: their base58 `address` and the same
/// address rendered as an `address_emoji` string.
#[frb]
#[derive(Debug, Clone)]
pub struct CounterpartyInfoDto {
    pub address: String,
    pub address_emoji: String,
}

impl From<tari_common_types::tari_address::TariAddress> for CounterpartyInfoDto {
    fn from(addr: tari_common_types::tari_address::TariAddress) -> Self {
        Self {
            address: addr.to_base58(),
            address_emoji: addr.to_emoji_string(),
        }
    }
}

/// Whether a transaction credited (`Incoming`) or debited (`Outgoing`) the wallet.
#[frb]
#[derive(Debug, Clone)]
pub enum DisplayedTransactionDirection {
    Incoming,
    Outgoing,
}

impl From<minotari_wallet::transactions::TransactionDirection> for DisplayedTransactionDirection {
    fn from(d: minotari_wallet::transactions::TransactionDirection) -> Self {
        match d {
            minotari_wallet::transactions::TransactionDirection::Incoming => Self::Incoming,
            minotari_wallet::transactions::TransactionDirection::Outgoing => Self::Outgoing,
        }
    }
}

/// How a transaction originated: a regular `Transfer`, mining `Coinbase`,
/// `OneSided` payment, or `Unknown`.
#[frb]
#[derive(Debug, Clone)]
pub enum DisplayedTransactionSource {
    Transfer,
    Coinbase,
    OneSided,
    Unknown,
}

impl From<minotari_wallet::transactions::TransactionSource> for DisplayedTransactionSource {
    fn from(s: minotari_wallet::transactions::TransactionSource) -> Self {
        match s {
            minotari_wallet::transactions::TransactionSource::Transfer => Self::Transfer,
            minotari_wallet::transactions::TransactionSource::Coinbase => Self::Coinbase,
            minotari_wallet::transactions::TransactionSource::OneSided => Self::OneSided,
            minotari_wallet::transactions::TransactionSource::Unknown => Self::Unknown,
        }
    }
}

/// The display status of a transaction as shown in the wallet UI.
#[frb]
#[derive(Debug, Clone)]
pub enum DisplayedTransactionStatus {
    Pending,
    Unconfirmed,
    Confirmed,
    Cancelled,
    Reorganized,
    Rejected,
    Locked,
}

impl From<minotari_wallet::transactions::TransactionDisplayStatus> for DisplayedTransactionStatus {
    fn from(s: minotari_wallet::transactions::TransactionDisplayStatus) -> Self {
        match s {
            minotari_wallet::transactions::TransactionDisplayStatus::Pending => Self::Pending,
            minotari_wallet::transactions::TransactionDisplayStatus::Unconfirmed => {
                Self::Unconfirmed
            }
            minotari_wallet::transactions::TransactionDisplayStatus::Confirmed => Self::Confirmed,
            minotari_wallet::transactions::TransactionDisplayStatus::Cancelled => Self::Cancelled,
            minotari_wallet::transactions::TransactionDisplayStatus::Reorganized => {
                Self::Reorganized
            }
            minotari_wallet::transactions::TransactionDisplayStatus::Rejected => Self::Rejected,
            minotari_wallet::transactions::TransactionDisplayStatus::Locked => Self::Locked,
        }
    }
}

/// A transaction as displayed in the wallet UI.
///
/// `amount` is in **microTari** with a pre-formatted `amount_display`. `counterparty`
/// and `fee` are absent for some kinds. `payrefs` are hex payment-reference hashes.
#[frb]
#[derive(Debug, Clone)]
pub struct DisplayedTransactionDto {
    pub id: String,
    pub direction: DisplayedTransactionDirection,
    pub source: DisplayedTransactionSource,
    pub status: DisplayedTransactionStatus,
    pub amount: u64,
    pub amount_display: String,
    pub message: Option<String>,
    pub counterparty: Option<CounterpartyInfoDto>,
    pub blockchain: BlockchainInfoDto,
    pub fee: Option<FeeInfoDto>,
    pub payrefs: Vec<String>,
}

impl From<minotari_wallet::DisplayedTransaction> for DisplayedTransactionDto {
    fn from(t: minotari_wallet::DisplayedTransaction) -> Self {
        // Incoming receipts need references for our received outputs, even when
        // the sender's optional memo contains no sent_output_hashes.
        let mut payrefs = Vec::new();
        let is_mined = t.blockchain.block_height > 0
            && t.blockchain.block_hash != FixedHash::zero()
            && !matches!(
                t.status,
                minotari_wallet::transactions::TransactionDisplayStatus::Reorganized
                    | minotari_wallet::transactions::TransactionDisplayStatus::Cancelled
                    | minotari_wallet::transactions::TransactionDisplayStatus::Rejected
            );
        if is_mined {
            if matches!(
                t.direction,
                minotari_wallet::transactions::TransactionDirection::Incoming
            ) {
                for output in &t.details.outputs {
                    if !output.is_change
                        && output.mined_in_block_height == t.blockchain.block_height
                        && output.mined_in_block_hash == t.blockchain.block_hash
                        && output.hash != FixedHash::zero()
                    {
                        let reference =
                            generate_payment_reference(&output.mined_in_block_hash, &output.hash)
                                .to_string();
                        if !payrefs.contains(&reference) {
                            payrefs.push(reference);
                        }
                    }
                }
            }
            // Pending sends were built with a zero block hash, so their stored
            // references are provisional. Rebuild from the actual mined block
            // and the recipient output hashes, never from our change outputs.
            if matches!(
                t.direction,
                minotari_wallet::transactions::TransactionDirection::Outgoing
            ) {
                payrefs = t
                    .details
                    .sent_output_hashes
                    .iter()
                    .filter(|hash| **hash != FixedHash::zero())
                    .map(|hash| {
                        generate_payment_reference(&t.blockchain.block_hash, hash).to_string()
                    })
                    .collect();
            }
            // Retain references for entries that have no sent output hashes.
            if payrefs.is_empty() {
                payrefs = t
                    .details
                    .sent_payrefs
                    .iter()
                    .filter(|reference| **reference != FixedHash::zero())
                    .map(ToString::to_string)
                    .collect();
            }
        }
        Self {
            id: t.id.to_string(),
            direction: t.direction.into(),
            source: t.source.into(),
            status: t.status.into(),
            amount: t.amount.0,
            amount_display: format_micro_tari(t.amount.0),
            message: t.message,
            counterparty: t.counterparty.map(CounterpartyInfoDto::from),
            blockchain: t.blockchain.into(),
            fee: t.fee.map(FeeInfoDto::from),
            payrefs,
        }
    }
}

/// List a page of the wallet's transactions, most recent first.
///
/// `limit`/`offset` paginate the result. Requires
/// [`initialize_database`](crate::api::db::initialize_database) first.
/// Synchronous; errors if the account does not exist.
#[frb]
pub fn get_transactions(
    wallet_name: String,
    limit: i64,
    offset: i64,
) -> Result<Vec<DisplayedTransactionDto>> {
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, Some(&wallet_name))?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;

    let transactions = get_displayed_transactions_paginated(&conn, account.id, limit, offset)?;

    transactions
        .into_iter()
        .map(|tx| history_dto(&conn, tx))
        .collect()
}

/// Read and acknowledge transactions changed by this process's scan/send events.
///
/// Returns current canonical history DTOs, including reorg changes, for this wallet
/// only. Empty cycles read no history rows. Failed reads retain pending IDs, and
/// concurrent updates remain pending. Load the full paginated history on opening:
/// this in-memory notification queue is not a persistent history or a cursor.
/// `transaction_ids` optionally includes IDs received in scan events; foreign or
/// missing IDs are ignored. It does not change the scan event contract.
#[frb]
pub fn get_changed_transactions(
    wallet_name: String,
    transaction_ids: Option<Vec<String>>,
) -> Result<Vec<DisplayedTransactionDto>> {
    let mut conn = get_db_connection()?;
    let account = get_accounts(&mut conn, Some(&wallet_name))?
        .into_iter()
        .next()
        .ok_or(WalletError::NoAccounts)?;
    if let Some(ids) = transaction_ids {
        record_transaction_changes(account.id, ids);
    }
    let read = HISTORY_CHANGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pending(account.id);
    let mut changed = Vec::with_capacity(read.len());
    for (id, _) in &read {
        let json: Option<String> = conn.query_row(
            "SELECT transaction_json FROM displayed_transactions WHERE account_id = ?1 AND id = ?2",
            (account.id, id), |row| row.get(0),
        ).optional()?;
        if let Some(json) = json {
            let tx = serde_json::from_str(&json).context("Invalid changed transaction history")?;
            changed.push(history_dto(&conn, tx)?);
        }
    }
    HISTORY_CHANGES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .acknowledge(account.id, &read);
    Ok(changed)
}

fn restore_mined_block_hash(
    conn: &Connection,
    tx: &mut minotari_wallet::DisplayedTransaction,
) -> Result<bool> {
    use minotari_wallet::transactions::TransactionDisplayStatus as Status;
    if tx.blockchain.block_hash != FixedHash::zero()
        || tx.blockchain.block_height == 0
        || matches!(
            tx.status,
            Status::Reorganized | Status::Cancelled | Status::Rejected
        )
    {
        return Ok(false);
    }
    let Ok(height) = i64::try_from(tx.blockchain.block_height) else {
        return Ok(false);
    };
    let mut hash: Option<Vec<u8>> = conn
        .query_row(
            "SELECT hash FROM scanned_tip_blocks WHERE account_id = ?1 AND height = ?2 LIMIT 1",
            (tx.details.account_id, height),
            |row| row.get(0),
        )
        .optional()?;
    // Old scanned blocks may have been pruned. A matched wallet output (usually
    // the change) retains the public hash of its mined block.
    if hash
        .as_ref()
        .is_none_or(|h| h.iter().all(|byte| *byte == 0))
    {
        for output in &tx.details.outputs {
            hash = conn.query_row(
                "SELECT mined_in_block_hash FROM outputs WHERE account_id = ?1
                 AND output_hash = ?2 AND mined_in_block_height = ?3 AND deleted_at IS NULL LIMIT 1",
                (tx.details.account_id, output.hash.as_bytes(), height), |row| row.get(0),
            ).optional()?;
            if hash
                .as_ref()
                .is_some_and(|h| h.iter().any(|byte| *byte != 0))
            {
                break;
            }
        }
    }
    if let Some(hash) = hash {
        let hash =
            FixedHash::try_from(hash).context("Invalid mined block hash in wallet history")?;
        if hash != FixedHash::zero() {
            tx.blockchain.block_hash = hash;
            return Ok(true);
        }
    }
    Ok(false)
}

fn history_dto(
    conn: &Connection,
    mut tx: minotari_wallet::DisplayedTransaction,
) -> Result<DisplayedTransactionDto> {
    restore_mined_block_hash(conn, &mut tx)?;
    if tx.direction == minotari_wallet::transactions::TransactionDirection::Outgoing {
        let mut normalized = false;
        if let Some(completed) = minotari_wallet::db::get_completed_transaction_by_id(conn, tx.id)?
        {
            if completed.account_id == tx.details.account_id {
                let signed: Transaction = serde_json::from_slice(&completed.serialized_transaction)
                    .context("Failed to read signed transaction for wallet history")?;
                normalize_local_send(&mut tx, kernel_fee(&signed)?);
                normalized = true;
            }
        }
        if !normalized {
            let mut recovered_fee = None;
            if let [output] = tx.details.outputs.as_slice() {
                if let Some((_, output_tx, change)) =
                    minotari_wallet::db::get_output_info_by_hash_for_account(
                        conn,
                        tx.details.account_id,
                        &output.hash,
                    )?
                {
                    if output_tx == tx.id {
                        recovered_fee = recovered_send_fee(&tx, &change);
                    }
                }
            }
            if let Some(fee) = recovered_fee {
                normalize_local_send(&mut tx, fee);
            } else {
                // No verified local kernel or consistent reconstruction: keep
                // the known net debit, without inventing a fee of zero.
                tx.fee = None;
            }
        }
    }
    Ok(tx.into())
}

/// Refresh mined Pending/Unconfirmed/Locked entries using this account's scanned tip.
///
/// Legacy compatibility API for minotari v0.3.0's skipped monitor. The pinned
/// main revision fixes that monitor; Cake now consumes scan updates instead.
/// Unmined sends and removed transactions are preserved.
/// Returns changed entries; existing scan events and read APIs are unchanged.
#[frb]
pub fn refresh_transaction_confirmations(
    wallet_name: String,
    required_confirmations: Option<u64>,
) -> Result<Vec<DisplayedTransactionDto>> {
    let required =
        required_confirmations.unwrap_or(crate::api::config::DEFAULT_CONFIRMATION_WINDOW);
    anyhow::ensure!(
        required > 0,
        "Required confirmations must be greater than zero"
    );
    let mut conn = get_db_connection()?;
    let account = get_accounts(&mut conn, Some(&wallet_name))?
        .into_iter()
        .next()
        .ok_or(WalletError::NoAccounts)?;
    let snapshot = conn.transaction()?;
    let Some(tip) = get_latest_scanned_tip_block_by_account(&snapshot, account.id)? else {
        return Ok(Vec::new());
    };
    let mut updated = Vec::new();
    for mut tx in get_displayed_transactions_needing_confirmation_update(&snapshot, account.id)? {
        let repaired_hash = restore_mined_block_hash(&snapshot, &mut tx)?;
        if refresh_mined_status(&mut tx, tip.height, required) || repaired_hash {
            update_displayed_transaction_confirmations(&snapshot, &tx)?;
            updated.push(history_dto(&snapshot, tx)?);
        }
    }
    snapshot.commit()?;
    Ok(updated)
}

fn refresh_mined_status(
    tx: &mut minotari_wallet::DisplayedTransaction,
    scanned_height: u64,
    required: u64,
) -> bool {
    use minotari_wallet::transactions::TransactionDisplayStatus as Status;
    if !matches!(
        tx.status,
        Status::Pending | Status::Unconfirmed | Status::Locked
    ) || tx.blockchain.block_height == 0
        || tx.blockchain.block_height > scanned_height
    {
        return false;
    }
    let confirmations = scanned_height.saturating_sub(tx.blockchain.block_height);
    let status = if confirmations >= required {
        if tx.lock_height > scanned_height {
            Status::Locked
        } else {
            Status::Confirmed
        }
    } else if confirmations > 0 {
        Status::Unconfirmed
    } else {
        Status::Pending
    };
    if confirmations == tx.blockchain.confirmations && status == tx.status {
        return false;
    }
    tx.blockchain.confirmations = confirmations;
    tx.status = status;
    true
}

#[cfg(test)]
mod tests {
    //! Enum-mapping tests. Pure conversions, no I/O. These are upstream-drift
    //! tripwires: if `minotari`'s transaction enums add/rename a variant, the
    //! `From` impls (and these exhaustive assertions) must be revisited.
    //!
    //! The DTO enums derive only `Debug`/`Clone` (no `PartialEq`), so equality is
    //! asserted with `matches!` against the expected variant.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use minotari_wallet::models::OutputStatus;
    use minotari_wallet::transactions::{
        BlockchainInfo, TransactionDetails, TransactionDirection, TransactionDisplayStatus,
        TransactionOutput, TransactionSource,
    };
    use tari_transaction_components::{
        tari_amount::MicroMinotari, transaction_components::OutputType,
    };
    use tari_utilities::hex::Hex;

    const RECEIVED_PAYREF: &str =
        "9cb2bb582664d051387c2d40ed638be2d7d98226b64ced71f90827a8e73f5a1a";

    #[test]
    fn an_outgoing_without_signed_or_reconstructed_fee_keeps_its_debit_and_unknown_fee() {
        let dir = tempfile::tempdir().unwrap();
        let pool = minotari_wallet::init_db(dir.path().join("wallet.db")).unwrap();
        let conn = pool.get().unwrap();
        let mut tx = received_transaction();
        tx.direction = TransactionDirection::Outgoing;
        tx.amount = MicroMinotari(25_000_700);
        tx.fee = Some(minotari_wallet::transactions::FeeInfo {
            amount: MicroMinotari(0),
        });
        let dto = history_dto(&conn, tx).unwrap();
        assert_eq!(dto.amount, 25_000_700);
        assert!(dto.fee.is_none(), "unknown must not be displayed as zero");
        let incoming = history_dto(&conn, received_transaction()).unwrap();
        assert_eq!(incoming.amount, 100_000_000);
        assert!(incoming.fee.is_none());
    }

    #[test]
    fn mined_receipt_crosses_confirmation_threshold_and_keeps_its_reference() {
        let mut tx = received_transaction();
        assert!(refresh_mined_status(&mut tx, 363576, 3));
        assert_eq!(tx.blockchain.confirmations, 2);
        assert_eq!(tx.status, TransactionDisplayStatus::Unconfirmed);
        assert!(refresh_mined_status(&mut tx, 363577, 3));
        assert_eq!(tx.status, TransactionDisplayStatus::Confirmed);
        assert_eq!(
            DisplayedTransactionDto::from(tx).payrefs,
            vec![RECEIVED_PAYREF]
        );
    }

    #[test]
    fn mined_lock_stays_locked_until_its_height_and_can_change_status_only() {
        let mut tx = received_transaction();
        tx.lock_height = 363580;
        assert!(refresh_mined_status(&mut tx, 363577, 3));
        assert_eq!(tx.status, TransactionDisplayStatus::Locked);
        assert!(!refresh_mined_status(&mut tx, 363577, 3));
        assert!(refresh_mined_status(&mut tx, 363580, 3));
        assert_eq!(tx.status, TransactionDisplayStatus::Confirmed);
        tx.status = TransactionDisplayStatus::Unconfirmed;
        assert!(refresh_mined_status(&mut tx, 363580, 3));
        assert_eq!(tx.status, TransactionDisplayStatus::Confirmed);
    }

    #[test]
    fn unmined_future_or_removed_entries_are_not_promoted() {
        for status in [
            TransactionDisplayStatus::Cancelled,
            TransactionDisplayStatus::Rejected,
            TransactionDisplayStatus::Reorganized,
            TransactionDisplayStatus::Confirmed,
        ] {
            let mut tx = received_transaction();
            tx.status = status;
            assert!(!refresh_mined_status(&mut tx, 363714, 3));
            assert_eq!(tx.status, status);
            assert_eq!(tx.blockchain.confirmations, 0);
        }
        let mut tx = received_transaction();
        tx.direction = TransactionDirection::Outgoing;
        tx.blockchain.block_height = 0;
        tx.status = TransactionDisplayStatus::Pending;
        assert!(!refresh_mined_status(&mut tx, 363714, 3));
        assert_eq!(tx.status, TransactionDisplayStatus::Pending);
        tx.blockchain.block_height = 363715;
        assert!(!refresh_mined_status(&mut tx, 363714, 3));
    }

    #[test]
    fn mined_send_with_upstreams_zero_block_hash_reaches_confirmed() {
        let mut tx = received_transaction();
        tx.direction = TransactionDirection::Outgoing;
        tx.status = TransactionDisplayStatus::Unconfirmed;
        tx.blockchain.block_hash = FixedHash::zero();
        assert!(refresh_mined_status(&mut tx, 363576, 3));
        assert_eq!(tx.blockchain.confirmations, 2);
        assert_eq!(tx.status, TransactionDisplayStatus::Unconfirmed);
        assert!(refresh_mined_status(&mut tx, 363577, 3));
        assert_eq!(tx.blockchain.confirmations, 3);
        assert_eq!(tx.status, TransactionDisplayStatus::Confirmed);
        // A known height is enough to count depth, but a real hash is still
        // necessary to construct a current payment reference.
        assert!(DisplayedTransactionDto::from(tx).payrefs.is_empty());
    }

    fn received_transaction() -> minotari_wallet::DisplayedTransaction {
        // Public output/block hashes from the funded integration wallet, also
        // checked against the explorer. No wallet keys or seed are required.
        let block_hash =
            FixedHash::from_hex("35222fd500aae8c471b37885dcded36767afde6ea7609daaedbbe9e01a47b442")
                .unwrap();
        let output_hash =
            FixedHash::from_hex("d672aebab31aa1cf521ddc1f85ba93587e6393acc1fca0a810f01c38211c3683")
                .unwrap();
        minotari_wallet::DisplayedTransaction {
            id: 42_u64.into(),
            direction: TransactionDirection::Incoming,
            source: TransactionSource::Transfer,
            status: TransactionDisplayStatus::Unconfirmed,
            amount: MicroMinotari(100_000_000),
            message: None,
            counterparty: None,
            blockchain: BlockchainInfo {
                block_height: 363574,
                timestamp: chrono::DateTime::from_timestamp(0, 0).unwrap().naive_utc(),
                confirmations: 0,
                block_hash,
            },
            fee: None,
            details: TransactionDetails {
                account_id: 1,
                total_credit: MicroMinotari(100_000_000),
                total_debit: MicroMinotari(0),
                inputs: vec![],
                outputs: vec![TransactionOutput {
                    hash: output_hash,
                    amount: MicroMinotari(100_000_000),
                    status: OutputStatus::Unspent,
                    mined_in_block_height: 363574,
                    mined_in_block_hash: block_hash,
                    output_type: OutputType::Standard,
                    is_change: false,
                }],
                output_type: None,
                coinbase_extra: None,
                memo_hex: None,
                sent_output_hashes: vec![],
                sent_payrefs: vec![],
            },
            lock_height: 0,
        }
    }

    #[test]
    fn incoming_without_sent_payrefs_exports_the_explorer_reference() {
        let dto = DisplayedTransactionDto::from(received_transaction());
        assert_eq!(dto.payrefs, vec![RECEIVED_PAYREF]);
        assert_eq!(dto.id, "42");
        assert!(matches!(
            dto.status,
            DisplayedTransactionStatus::Unconfirmed
        ));
    }

    #[test]
    fn incoming_exports_all_received_outputs_without_change_or_duplicates() {
        let mut tx = received_transaction();
        let first = tx.details.outputs[0].clone();
        let mut second = first.clone();
        second.hash = FixedHash::from([2_u8; 32]);
        let second_reference =
            generate_payment_reference(&second.mined_in_block_hash, &second.hash).to_string();
        let mut change = first.clone();
        change.hash = FixedHash::from([3_u8; 32]);
        change.is_change = true;
        tx.details.outputs.extend([first, second, change]);
        tx.details.sent_payrefs.push(FixedHash::from([7_u8; 32]));
        assert_eq!(
            DisplayedTransactionDto::from(tx).payrefs,
            vec![RECEIVED_PAYREF.to_string(), second_reference]
        );
    }

    #[test]
    fn unmined_or_removed_transactions_have_no_current_payrefs() {
        let mut unmined = received_transaction();
        unmined.blockchain.block_height = 0;
        assert!(DisplayedTransactionDto::from(unmined).payrefs.is_empty());

        let mut unknown_block = received_transaction();
        unknown_block.blockchain.block_hash = FixedHash::zero();
        assert!(DisplayedTransactionDto::from(unknown_block)
            .payrefs
            .is_empty());

        for status in [
            TransactionDisplayStatus::Reorganized,
            TransactionDisplayStatus::Cancelled,
            TransactionDisplayStatus::Rejected,
        ] {
            let mut tx = received_transaction();
            tx.status = status;
            tx.details.sent_payrefs.push(FixedHash::from([8_u8; 32]));
            assert!(DisplayedTransactionDto::from(tx).payrefs.is_empty());
        }

        let mut stale_output = received_transaction();
        stale_output.details.outputs[0].mined_in_block_hash = FixedHash::from([9_u8; 32]);
        assert!(DisplayedTransactionDto::from(stale_output)
            .payrefs
            .is_empty());
    }

    #[test]
    fn outgoing_preserves_sent_references_instead_of_our_received_outputs() {
        let mut tx = received_transaction();
        tx.direction = TransactionDirection::Outgoing;
        let reference = FixedHash::from([8_u8; 32]);
        tx.details.sent_payrefs = vec![reference];
        assert_eq!(
            DisplayedTransactionDto::from(tx).payrefs,
            vec![reference.to_string()]
        );
    }

    #[test]
    fn mined_send_replaces_provisional_payref_with_recipient_reference() {
        let mut tx = received_transaction();
        tx.direction = TransactionDirection::Outgoing;
        let sent_hash = FixedHash::from([8_u8; 32]);
        tx.details.sent_output_hashes = vec![sent_hash];
        let provisional = generate_payment_reference(&FixedHash::zero(), &sent_hash);
        tx.details.sent_payrefs = vec![provisional];
        let mined = generate_payment_reference(&tx.blockchain.block_hash, &sent_hash).to_string();
        assert_ne!(mined, provisional.to_string());
        assert_eq!(DisplayedTransactionDto::from(tx).payrefs, vec![mined]);
    }

    #[test]
    fn direction_maps_exhaustively() {
        assert!(matches!(
            DisplayedTransactionDirection::from(TransactionDirection::Incoming),
            DisplayedTransactionDirection::Incoming
        ));
        assert!(matches!(
            DisplayedTransactionDirection::from(TransactionDirection::Outgoing),
            DisplayedTransactionDirection::Outgoing
        ));
    }

    #[test]
    fn source_maps_exhaustively() {
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::Transfer),
            DisplayedTransactionSource::Transfer
        ));
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::Coinbase),
            DisplayedTransactionSource::Coinbase
        ));
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::OneSided),
            DisplayedTransactionSource::OneSided
        ));
        assert!(matches!(
            DisplayedTransactionSource::from(TransactionSource::Unknown),
            DisplayedTransactionSource::Unknown
        ));
    }

    #[test]
    fn status_maps_exhaustively() {
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Pending),
            DisplayedTransactionStatus::Pending
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Unconfirmed),
            DisplayedTransactionStatus::Unconfirmed
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Confirmed),
            DisplayedTransactionStatus::Confirmed
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Cancelled),
            DisplayedTransactionStatus::Cancelled
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Reorganized),
            DisplayedTransactionStatus::Reorganized
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Rejected),
            DisplayedTransactionStatus::Rejected
        ));
        assert!(matches!(
            DisplayedTransactionStatus::from(TransactionDisplayStatus::Locked),
            DisplayedTransactionStatus::Locked
        ));
    }
}
