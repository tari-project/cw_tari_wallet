//! Burn Minotari (L1) to a Tari Ootle (L2) account, and hand the claim proof to L2.
//!
//! Thin adapter over minotari's burn builder (`transactions::burn`). The flow:
//!
//! 1. [`burn_to_l2`] builds, records and broadcasts a burn addressed to an Ootle account
//!    public key. Recording stores a partial proof in the wallet DB (`burn_proofs`).
//! 2. Once the burn is mined and buried under the confirmation window,
//!    [`get_burn_proofs`] fetches the burn output proof from the base node, assembles the
//!    complete claim proof, writes it to `<db dir>/burn_proofs/` and returns its JSON.
//!    That JSON is what the Ootle wallet claims with.
//!
//! The daemon's background `BurnProofWorker` is not run here: the bridge has no
//! long-lived process to host it, so the same work happens on demand inside
//! [`get_burn_proofs`].

use crate::api::config::{
    BURN_PROOFS_DIR, DEFAULT_BASE_URL, DEFAULT_CONFIRMATION_WINDOW, DEFAULT_PASSPHRASE,
    SECONDS_TO_LOCK_UTXO, SEND_FEE_PER_GRAM,
};
use crate::api::db::{get_db_connection, get_db_path, release_expired_reservations};
use crate::api::error::WalletError;
use crate::api::network::{apply_network, parse_network, TariNetwork};
use crate::api::send_transaction::{SendTransactionEvent, TransactionStage};
use crate::domain::burn::{
    assemble_claim_proof, is_confirmed, parse_claim_public_key, proof_file_name,
};
use crate::frb_generated::StreamSink;
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::db::{
    expire_and_unlock_pending_transaction, find_pending_transaction_record_by_idempotency_key,
    get_account_by_name, mark_burn_proof_complete, DbBurnProof,
};
use minotari_wallet::http::WalletHttpClient;
use minotari_wallet::transactions::burn::{create_burn_tx, persist_burn_records, BurnTxParams};
use minotari_wallet::transactions::manager::record_first_broadcast;
use r2d2_sqlite::rusqlite::Connection;
use std::path::{Path, PathBuf};
use tari_common::configuration::Network;
use tari_common_types::types::FixedHash;
use tari_transaction_components::MicroMinotari;
use tari_utilities::hex::Hex;
use zeroize::Zeroizing;

/// Inputs to [`burn_to_l2`].
///
/// `passphrase` (**secret**) decrypts the stored wallet (`None` → empty), which signs
/// the burn; a view-only wallet cannot burn. `network` selects the Tari network
/// (`None` → MainNet); `base_url` is the base node RPC endpoint (`None` → the default
/// mainnet RPC). `claim_public_key` is the **Ootle account public key** the burn is
/// claimable by, as 64 hex characters; it must be a valid Ristretto point. A wrong key
/// loses the funds for good, so take it from the L2 wallet, never from user typing.
/// `amount` is in **microTari**. `fee_per_gram` is in µT (`None` → 5; pass
/// `estimate_transaction_fee(...).fee_per_gram` to honour a fee priority).
/// `confirmation_window` is the required confirmations for spent inputs (`None` → 3).
///
/// The burn is always for the Ootle itself: no sidechain deployment key is set.
#[frb]
pub struct BurnToL2Details {
    pub passphrase: Option<String>,
    pub network: Option<TariNetwork>,
    pub base_url: Option<String>,
    pub wallet_name: String,
    pub claim_public_key: String,
    pub amount: u64,
    pub fee_per_gram: Option<u64>,
    pub payment_id: Option<String>,
    pub confirmation_window: Option<u64>,
}

/// A recorded burn, as returned by [`burn_to_l2`].
///
/// `output_hash` and `commitment` (hex) identify the burn's claim proof in
/// [`get_burn_proofs`]. `fee` is the kernel fee in µT. When `accepted_by_node` is
/// `false` the burn is still recorded and is rebroadcast by the scan: **do not burn
/// again**. `note`/`node_reason` explain why.
#[frb]
#[derive(Clone, Debug)]
pub struct BurnTransactionDto {
    pub tx_id: String,
    pub output_hash: String,
    pub commitment: String,
    pub claim_public_key: String,
    pub amount: u64,
    pub fee: u64,
    pub accepted_by_node: bool,
    pub note: Option<String>,
    pub node_reason: Option<String>,
}

/// Where a burn's claim proof is.
#[frb]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurnProofStatus {
    /// Not mined yet, not yet buried under the confirmation window, or the base node
    /// could not produce the proof; see `pending_reason`. Retried on the next call.
    Pending,
    /// `proof_json` holds the complete claim proof.
    Ready,
}

/// One burn's claim proof, from [`get_burn_proofs`].
///
/// `proof_json` (when `Ready`) is a `CompleteClaimBurnProof`
/// (`{"claim_proof":{...},"encrypted_data":"<base64>","mined_in_epoch":N}`), the same
/// JSON minotari's burn proof worker writes and the Ootle wallet daemon reads. The
/// `ootle-rs` `ClaimBurn` takes its `claim_proof` and `encrypted_data`.
#[frb]
#[derive(Clone, Debug)]
pub struct BurnProofDto {
    pub output_hash: String,
    pub commitment: String,
    pub claim_public_key: String,
    pub amount: u64,
    pub status: BurnProofStatus,
    pub proof_json: Option<String>,
    pub pending_reason: Option<String>,
}

#[frb(ignore)]
pub async fn burn_to_l2_with_handler<F>(
    mut details: BurnToL2Details,
    status_callback: F,
) -> Result<BurnTransactionDto>
where
    F: Fn(SendTransactionEvent) + Send + Sync + 'static,
{
    let passphrase = Zeroizing::new(
        details
            .passphrase
            .take()
            .unwrap_or_else(|| DEFAULT_PASSPHRASE.to_string()),
    );
    let report = |stage: TransactionStage, msg: &str| {
        status_callback(SendTransactionEvent {
            stage,
            details: msg.to_string(),
        });
    };

    report(TransactionStage::Initializing, "Starting burn...");

    report(TransactionStage::ValidatingInput, "Parsing inputs...");
    let network = parse_network(details.network);
    let claim_public_key = parse_claim_public_key(&details.claim_public_key)?;
    if details.amount == 0 {
        return Err(WalletError::wallet("Amount must be greater than zero").into());
    }
    apply_network(network)?;

    report(
        TransactionStage::ConnectingToNetwork,
        "Accessing wallet database...",
    );
    // Built before anything is locked: once the burn is recorded no failure may be
    // reported as an error (it would invite a second burn).
    let client = base_node_client(details.base_url.as_deref())?;
    let mut conn = get_db_connection()?;
    release_expired_reservations(&conn);
    let account = get_account_by_name(&conn, &details.wallet_name)
        .map_err(|e| WalletError::database(e.to_string()))?
        .ok_or(WalletError::NoAccounts)?;

    report(
        TransactionStage::ConstructingTransaction,
        "Building and signing burn...",
    );
    let idempotency_key = uuid::Uuid::new_v4().to_string();
    let params = BurnTxParams {
        account_id: account.id,
        amount: MicroMinotari(details.amount),
        claim_public_key: Some(claim_public_key),
        sidechain_deployment_key: None,
        fee_per_gram: MicroMinotari(details.fee_per_gram.unwrap_or(SEND_FEE_PER_GRAM)),
        payment_id: details.payment_id.take(),
        idempotency_key: Some(idempotency_key.clone()),
        seconds_to_lock: SECONDS_TO_LOCK_UTXO,
        confirmation_window: details
            .confirmation_window
            .unwrap_or(DEFAULT_CONFIRMATION_WINDOW),
    };
    // Nothing is broadcast until both succeed, and `persist_burn_records` commits all or
    // nothing, so on failure the reserved inputs can be handed straight back.
    let result = create_burn_tx(&account, &mut conn, network, &passphrase, params)
        .and_then(|result| {
            persist_burn_records(&mut conn, &result, account.id, &idempotency_key)?;
            Ok(result)
        })
        .map_err(|e| {
            release_burn_reservation(&conn, account.id, &idempotency_key);
            WalletError::wallet(format!("Failed to build burn: {e}"))
        })?;
    drop(conn);

    let dto_base = BurnTransactionDto {
        tx_id: result.tx_id.to_string(),
        output_hash: result.output_hash.to_hex(),
        commitment: result.new_burn_proof.commitment.to_hex(),
        claim_public_key: result.new_burn_proof.claim_public_key.clone(),
        amount: details.amount,
        fee: result.new_burn_proof.kernel_fee,
        accepted_by_node: false,
        note: None,
        node_reason: None,
    };

    report(TransactionStage::Broadcasting, "Broadcasting burn...");
    let response = client.submit_transaction(result.transaction).await;
    // Never fails: the burn is recorded and the scan's monitor rebroadcasts it.
    let conn = get_db_connection().ok();
    let first = record_first_broadcast(conn.as_deref(), result.tx_id, response);

    report(TransactionStage::Completed, "Burn recorded");
    Ok(BurnTransactionDto {
        accepted_by_node: first.accepted_by_node,
        note: first.note(),
        node_reason: first.node_reason,
        ..dto_base
    })
}

/// Burn `amount` µT to an Ootle (L2) account, streaming progress.
///
/// Streams [`SendTransactionEvent`]s (`Initializing`, `ValidatingInput`,
/// `ConnectingToNetwork`, `ConstructingTransaction`, `Broadcasting`, `Completed`) over
/// `sink` and resolves to the recorded [`BurnTransactionDto`]. Requires
/// [`initialize_database`](crate::api::db::initialize_database) first.
///
/// **A burn is irreversible.** `Ok` means it is recorded, not necessarily accepted by
/// the node; the scan's transaction monitor rebroadcasts it (only while `start_scan`
/// runs), so **never burn again after `Ok`**. An `Err` means nothing was broadcast.
/// Like `send_transaction`, the burn continues if the Dart stream is closed.
///
/// Fetch the claim proof later with [`get_burn_proofs`].
#[frb]
pub async fn burn_to_l2(
    sink: StreamSink<SendTransactionEvent>,
    details: BurnToL2Details,
) -> Result<BurnTransactionDto> {
    burn_to_l2_with_handler(details, move |event| {
        // Ignore sink-closed: a half-broadcast burn must still finish.
        let _ = sink.add(event);
    })
    .await
}

/// List `wallet_name`'s burns with their claim proofs, producing any proof now ready.
///
/// For each burn without a stored proof, asks the base node at `base_url` (`None` → the
/// default mainnet RPC) for the burn output proof. Once the burn's block is 3 blocks
/// deep the complete proof is written to `<db dir>/burn_proofs/` (so it survives a
/// pruned node forgetting the block) and returned as `Ready`; otherwise the burn stays
/// `Pending` with a reason. Call it again after the burn confirms (e.g. after a scan).
/// `network` (`None` → MainNet) sets the epoch length for `mined_in_epoch`.
///
/// A node that has pruned the burn's block cannot prove it; point `base_url` at an
/// archival node to recover such a proof.
#[frb]
pub async fn get_burn_proofs(
    wallet_name: String,
    network: Option<TariNetwork>,
    base_url: Option<String>,
) -> Result<Vec<BurnProofDto>> {
    let network = parse_network(network);
    let proofs_dir = burn_proofs_dir(&get_db_path()?);
    let proofs = {
        let conn = get_db_connection()?;
        let account = get_account_by_name(&conn, &wallet_name)
            .map_err(|e| WalletError::database(e.to_string()))?
            .ok_or(WalletError::NoAccounts)?;
        burn_proofs_for_account(&conn, account.id)
            .map_err(|e| WalletError::database(e.to_string()))?
    };

    let mut client = None;
    let mut tip_height = None;
    let mut out = Vec::with_capacity(proofs.len());
    for proof in proofs {
        let path = proofs_dir.join(proof_file_name(&proof));
        let result = match std::fs::read_to_string(&path) {
            Ok(json) => Ok(json),
            Err(_) => {
                let client = match &client {
                    Some(c) => c,
                    None => client.insert(base_node_client(base_url.as_deref())?),
                };
                refresh_proof(client, &mut tip_height, &proof, network, &path).await
            }
        };
        out.push(BurnProofDto {
            output_hash: proof.output_hash.to_hex(),
            commitment: proof.commitment.to_hex(),
            claim_public_key: proof.claim_public_key.clone(),
            amount: u64::try_from(proof.value).unwrap_or_default(),
            status: if result.is_ok() {
                BurnProofStatus::Ready
            } else {
                BurnProofStatus::Pending
            },
            pending_reason: result.as_ref().err().cloned(),
            proof_json: result.ok(),
        });
    }
    Ok(out)
}

/// Fetch, check, assemble and persist one burn's claim proof. `Err` is the reason it
/// is still pending.
async fn refresh_proof(
    client: &WalletHttpClient,
    tip_height: &mut Option<u64>,
    proof: &DbBurnProof,
    network: Network,
    path: &Path,
) -> Result<String, String> {
    let output_proof = client
        .get_burn_output_proof(&proof.commitment)
        .await
        .map_err(|e| format!("Burn output proof not available yet: {e}"))?
        .proof;

    let tip = match tip_height {
        Some(tip) => *tip,
        None => {
            let tip = client
                .get_tip_info()
                .await
                .map_err(|e| format!("Could not read the chain tip: {e}"))?
                .metadata
                .ok_or("The base node reported no chain tip")?
                .best_block_height();
            *tip_height.insert(tip)
        }
    };
    if !is_confirmed(tip, output_proof.block_height, DEFAULT_CONFIRMATION_WINDOW) {
        return Err(format!(
            "Burn mined at height {} is waiting for {} confirmations (tip {tip})",
            output_proof.block_height, DEFAULT_CONFIRMATION_WINDOW
        ));
    }

    let json = assemble_claim_proof(proof, output_proof, network).map_err(|e| e.to_string())?;
    write_atomically(path, &json).map_err(|e| format!("Failed to save the burn proof: {e}"))?;

    // The file is the record; the status only mirrors it for minotari's own worker.
    match get_db_connection() {
        Ok(conn) => {
            if let Err(e) = mark_burn_proof_complete(&conn, proof.id) {
                log::warn!("Saved a burn proof but could not mark it complete: {e}");
            }
        }
        Err(e) => log::warn!("Saved a burn proof but could not mark it complete: {e}"),
    }
    Ok(json)
}

/// Write via a temp file + rename so a crash never leaves a truncated proof that a
/// later call would report as `Ready`.
fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

fn burn_proofs_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(BURN_PROOFS_DIR)
}

fn base_node_client(base_url: Option<&str>) -> Result<WalletHttpClient> {
    let url = base_url
        .unwrap_or(DEFAULT_BASE_URL)
        .parse()
        .map_err(|e| WalletError::network(format!("Invalid base node URL: {e}")))?;
    WalletHttpClient::new(url).map_err(|e| WalletError::network(e.to_string()).into())
}

/// Hand back the inputs a burn reserved but never broadcast, best-effort.
fn release_burn_reservation(conn: &Connection, account_id: i64, idempotency_key: &str) {
    let released =
        find_pending_transaction_record_by_idempotency_key(conn, idempotency_key, account_id)
            .and_then(|record| match record {
                Some(record) => expire_and_unlock_pending_transaction(conn, &record.id),
                None => Ok(false),
            });
    if let Err(e) = released {
        log::warn!(
            "Failed to release the UTXO reservation for a failed burn; it is swept once it expires: {e}"
        );
    }
}

/// Every burn proof row of one account, oldest first. minotari only lists pending rows
/// across all accounts, so the query lives here.
fn burn_proofs_for_account(
    conn: &Connection,
    account_id: i64,
) -> r2d2_sqlite::rusqlite::Result<Vec<DbBurnProof>> {
    let mut stmt = conn.prepare(
        "SELECT id, account_id, output_hash, commitment, claim_public_key,
                ownership_proof_nonce, ownership_proof_sig, encrypted_data, value, status
         FROM burn_proofs WHERE account_id = ?1 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([account_id], |row| {
        let output_hash: Vec<u8> = row.get(2)?;
        Ok(DbBurnProof {
            id: row.get(0)?,
            account_id: row.get(1)?,
            output_hash: FixedHash::try_from(output_hash.as_slice()).unwrap_or_default(),
            commitment: row.get(3)?,
            claim_public_key: row.get(4)?,
            ownership_proof_nonce: row.get(5)?,
            ownership_proof_sig: row.get(6)?,
            // Vestigial upstream: the claim proof no longer uses the kernel fields.
            kernel_excess: Vec::new(),
            kernel_excess_nonce: Vec::new(),
            kernel_excess_sig: Vec::new(),
            sender_offset_public_key: Vec::new(),
            encrypted_data: row.get(7)?,
            value: row.get(8)?,
            kernel_fee: 0,
            kernel_lock_height: 0,
            status: row.get(9)?,
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use minotari_wallet::db::{create_account, init_db, insert_burn_proof, NewBurnProof};
    use tari_common_types::seeds::cipher_seed::CipherSeed;
    use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};

    /// Exception to the DB-free unit-test rule: the account query is hand-written SQL
    /// over minotari's `burn_proofs` table, and a mis-mapped column would assemble a
    /// claim proof from the wrong bytes. Round-trip it through upstream's own insert.
    #[test]
    fn burn_proofs_for_account_reads_back_what_minotari_stored() {
        let dir = tempfile::tempdir().unwrap();
        let pool = init_db(dir.path().join("wallet.db")).unwrap();
        let conn = pool.get().unwrap();
        for name in ["a", "b"] {
            let wallet = WalletType::SeedWords(
                SeedWordsWallet::construct_new(CipherSeed::random()).unwrap(),
            );
            create_account(&conn, name, &wallet, "pass").unwrap();
        }
        let a = get_account_by_name(&conn, "a").unwrap().unwrap().id;
        let b = get_account_by_name(&conn, "b").unwrap().unwrap().id;

        let new = |account_id, n: u8| NewBurnProof {
            account_id,
            output_hash: FixedHash::from([n; 32]),
            commitment: vec![n + 1; 32],
            claim_public_key: "ab".repeat(32),
            ownership_proof_nonce: vec![n + 2; 32],
            ownership_proof_sig: vec![n + 3; 32],
            kernel_excess: vec![0; 32],
            kernel_excess_nonce: vec![0; 32],
            kernel_excess_sig: vec![0; 32],
            sender_offset_public_key: vec![0; 32],
            encrypted_data: vec![n + 4; 5],
            value: 1_000 + u64::from(n),
            kernel_fee: 7,
            kernel_lock_height: 0,
        };
        insert_burn_proof(&conn, &new(a, 10)).unwrap();
        insert_burn_proof(&conn, &new(b, 20)).unwrap();
        insert_burn_proof(&conn, &new(a, 30)).unwrap();

        let rows = burn_proofs_for_account(&conn, a).unwrap();
        assert_eq!(rows.len(), 2, "only this account's burns");
        let row = &rows[0];
        assert_eq!(row.output_hash, FixedHash::from([10; 32]));
        assert_eq!(row.commitment, vec![11; 32]);
        assert_eq!(row.claim_public_key, "ab".repeat(32));
        assert_eq!(row.ownership_proof_nonce, vec![12; 32]);
        assert_eq!(row.ownership_proof_sig, vec![13; 32]);
        assert_eq!(row.encrypted_data, vec![14; 5]);
        assert_eq!(row.value, 1_010);
        assert_eq!(row.status, "pending_merkle");
        assert_eq!(rows[1].output_hash, FixedHash::from([30; 32]));
    }

    #[test]
    fn proofs_live_next_to_the_wallet_db() {
        assert_eq!(
            burn_proofs_dir(Path::new("/data/wallet/wallet.db")),
            PathBuf::from("/data/wallet/burn_proofs")
        );
    }
}
