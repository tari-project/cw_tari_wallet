use crate::api::config::{DEFAULT_BASE_URL, DEFAULT_PASSPHRASE, SECONDS_TO_LOCK_UTXO};
use crate::api::db::{get_db_connection, get_db_pool, release_expired_reservations};
use crate::api::error::WalletError;
use crate::api::network::{apply_network, parse_network, TariNetwork};
use crate::api::transactions::DisplayedTransactionDto;
use crate::domain::history::{kernel_fee, normalize_local_send};
use crate::domain::keys::key_manager_from_seed_words;
use crate::domain::validation::{validate_send_inputs, ValidatedInputs};
use crate::frb_generated::StreamSink;
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::db::expire_and_unlock_pending_transaction;
use minotari_wallet::get_accounts;
use minotari_wallet::transactions::manager::TransactionSender;
use minotari_wallet::transactions::one_sided_transaction::Recipient;
use tari_common::configuration::Network;
use tari_common_types::tari_address::TariAddress;
use tari_transaction_components::consensus::ConsensusConstantsBuilder;
use tari_transaction_components::key_manager::{KeyManager, TransactionKeyManagerInterface};
use tari_transaction_components::offline_signing::models::PrepareOneSidedTransactionForSigningResult;
use tari_transaction_components::offline_signing::sign_locked_transaction;
use tari_transaction_components::transaction_builder::TransactionBuilderError;
use tari_transaction_components::MicroMinotari;
use zeroize::Zeroizing;

/// Inputs to [`send_transaction`].
///
/// `seed_words` (**secret**) and `passphrase` (**secret**) authorize spending.
/// `network` selects the Tari network (`None` → MainNet); `base_url` is the base
/// node RPC endpoint (`None` → the default mainnet RPC); `wallet_name` selects the
/// account; `recipient_address` is the recipient's base58 Tari address; `amount`
/// is in **microTari** (µT, 1e-6 XTM); `payment_id` is an optional tag;
/// `confirmation_window` is the required confirmations in blocks (`None` → 3).
#[frb]
pub struct SendTransactionDetails {
    pub seed_words: Vec<String>,
    pub passphrase: Option<String>,
    pub network: Option<TariNetwork>,
    pub base_url: Option<String>,
    pub wallet_name: String,
    pub recipient_address: String,
    pub amount: u64,
    pub payment_id: Option<String>,
    pub confirmation_window: Option<u64>,
}

/// The lifecycle stage of an in-flight send, carried on each
/// [`SendTransactionEvent`]. Emitted in order from `Initializing` to `Completed`.
#[frb]
#[derive(Clone, Debug)]
pub enum TransactionStage {
    Initializing,
    ValidatingInput,
    ConnectingToNetwork,
    // Reserved, frozen-contract variant: currently NOT emitted by the send flow
    // (it goes ConnectingToNetwork -> ConstructingTransaction). Kept because removing
    // it would break the Dart enum; do not rely on receiving it.
    FetchingBalance,
    ConstructingTransaction,
    SigningKeyGeneration,
    SigningTransaction,
    Broadcasting,
    Completed,
}

/// A progress event streamed during [`send_transaction`]: the current
/// [`TransactionStage`] plus a human-readable `details` message.
#[frb]
#[derive(Clone)]
pub struct SendTransactionEvent {
    pub stage: TransactionStage,
    pub details: String,
}

#[frb(ignore)]
pub async fn send_transaction_with_handler<F>(
    details: SendTransactionDetails,
    status_callback: F,
) -> Result<DisplayedTransactionDto>
where
    F: Fn(SendTransactionEvent) + Send + Sync + 'static,
{
    send_transaction_with_fee_handler(details, None, status_callback).await
}

/// Internal runner shared by the frozen send API and the explicit fee API.
#[frb(ignore)]
pub async fn send_transaction_with_fee_handler<F>(
    mut details: SendTransactionDetails,
    fee_limits: Option<(u64, u64)>,
    status_callback: F,
) -> Result<DisplayedTransactionDto>
where
    F: Fn(SendTransactionEvent) + Send + Sync + 'static,
{
    // Move the two secrets *out* of the caller-owned `details` and into zeroizing
    // containers before anything else runs (Shared Contracts §3). These are moves,
    // not clones, so the original heap buffers are the ones that get wiped when
    // these drop — on every return path, including the `?` early exits below.
    // `SendTransactionDetails`' public field *types* are unchanged, so this is not a
    // contract change. What remains outside our control is whatever copy the FFI
    // layer itself holds; see the proposal in CONTRIBUTING.md.
    let seed_words = Zeroizing::new(std::mem::take(&mut details.seed_words));
    let passphrase = Zeroizing::new(
        details
            .passphrase
            .take()
            .unwrap_or_else(|| DEFAULT_PASSPHRASE.to_string()),
    );

    if let Some((rate, maximum)) = fee_limits {
        minotari_wallet::transactions::fund_locker::validate_fee_per_gram(MicroMinotari(rate))?;
        if maximum == 0 {
            anyhow::bail!("Maximum transaction fee must be greater than zero");
        }
    }

    let report = |stage: TransactionStage, msg: &str| {
        status_callback(SendTransactionEvent {
            stage,
            details: msg.to_string(),
        });
    };

    report(TransactionStage::Initializing, "Starting workflow...");

    report(TransactionStage::ValidatingInput, "Parsing inputs...");
    let validated = validate_inputs(&details)?;

    apply_network(validated.network)?;

    report(
        TransactionStage::ConnectingToNetwork,
        "Accessing wallet database...",
    );
    let mut sender = create_transaction_sender(
        &details.wallet_name,
        &passphrase,
        validated.network,
        validated.confirmations,
    )?;
    if let Some((rate, _)) = fee_limits {
        sender.fee_per_gram = MicroMinotari(rate);
    }

    // Derive the signing key manager and prove it belongs to this account *before*
    // `start_new_transaction` reserves any UTXO. Since the bump, `sign_locked_transaction`
    // verifies a payload-integrity signature made with the preparing wallet's view key,
    // so a seed/account mismatch fails at signing — by which point the inputs are already
    // `Locked` and must be released again (see `release_send_reservation`). Checking first
    // makes that mismatch cost nothing.
    //
    // No stage event is emitted here: the streamed `TransactionStage` sequence is frozen
    // contract, so `SigningKeyGeneration` stays at its original position below.
    let key_manager = key_manager_from_seed_words(&seed_words)?;
    verify_seed_words_match_account(&details.wallet_name, &passphrase, &key_manager)?;

    // Best-effort: free inputs a stale reservation still holds before selecting new ones.
    match get_db_connection() {
        Ok(conn) => release_expired_reservations(&conn),
        Err(e) => log::warn!("Could not check for expired UTXO reservations: {e}"),
    }

    report(
        TransactionStage::ConstructingTransaction,
        "Building transaction UTXOs...",
    );
    // From here on UTXOs are reserved. Upstream releases them on most of its failure
    // paths; the bridge releases on a signing failure and again as a backstop on a
    // finalize failure, which does nothing once the send is recorded.
    let (pending_transaction_id, unsigned_tx) = build_unsigned_transaction(
        &mut sender,
        validated.recipient_address,
        validated.amount,
        details.payment_id.take(),
    )?;

    report(
        TransactionStage::SigningKeyGeneration,
        "Deriving keys from seed...",
    );

    report(
        TransactionStage::SigningTransaction,
        "Signing transaction...",
    );

    let consensus_constants = ConsensusConstantsBuilder::new(validated.network).build();

    let signed_transaction = match sign_locked_transaction(
        &key_manager,
        consensus_constants,
        validated.network,
        unsigned_tx,
    ) {
        Ok(tx) => tx,
        Err(e) => {
            release_send_reservation(&pending_transaction_id);
            return Err(map_signing_error(e).into());
        }
    };

    let actual_fee = match kernel_fee(&signed_transaction.signed_transaction.transaction) {
        Ok(fee) => fee,
        Err(error) => {
            release_send_reservation(&pending_transaction_id);
            return Err(error);
        }
    };
    if let Some((_, maximum)) = fee_limits {
        if let Err(error) = validate_confirmed_fee(actual_fee, maximum) {
            release_send_reservation(&pending_transaction_id);
            return Err(error);
        }
    }
    report(TransactionStage::Broadcasting, "Broadcasting to network...");

    let base_url = details
        .base_url
        .take()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let mut result_tx = match sender
        .finalize_transaction_and_broadcast(signed_transaction, base_url)
        .await
    {
        Ok(sent) => {
            // Finalization has recorded the send even when the first broadcast
            // was not accepted. Treat it as pending, never invite a duplicate send.
            if !sent.accepted_by_node {
                log::warn!(
                    "Send recorded but not yet accepted by the node: note={:?}, node_reason={:?}",
                    sent.note,
                    sent.node_reason
                );
            }
            sent.displayed_transaction
        }
        Err(e) => {
            release_send_reservation(&pending_transaction_id);
            return Err(WalletError::network(e.to_string()).into());
        }
    };

    crate::api::transactions::record_transaction_changes(
        result_tx.details.account_id,
        [result_tx.id.to_string()],
    );
    report(TransactionStage::Completed, "Transaction sent");

    normalize_local_send(&mut result_tx, actual_fee);
    Ok(result_tx.into())
}

/// Release the UTXOs this send reserved, best-effort.
///
/// `start_new_transaction` flips the selected outputs to `Locked` and commits. The only
/// other release in this embedding is the expiry sweep (`release_expired_reservations`,
/// run at database init and at the start of each send and scan), which waits out
/// `SECONDS_TO_LOCK_UTXO`; this call hands them back straight away.
///
/// `pending_transaction_id` is the `pending_transactions` row id (not the idempotency
/// key). `expire_and_unlock_pending_transaction` only acts while that row is still
/// `Pending`, so it cannot hand back the inputs of a transaction that was recorded.
///
/// Best-effort by design: this runs on an error path that already has a cause worth
/// reporting, so a failure here is logged rather than allowed to mask the original error.
fn release_send_reservation(pending_transaction_id: &str) {
    let released = get_db_connection()
        .map_err(|e| e.to_string())
        .and_then(|conn| {
            expire_and_unlock_pending_transaction(&conn, pending_transaction_id)
                .map_err(|e| e.to_string())
        });

    match released {
        Ok(true) => log::info!("Released the UTXO reservation for a failed send"),
        // Already `Completed`/`Expired`: nothing to release, which is the safe outcome.
        Ok(false) => log::debug!("No pending UTXO reservation to release for a failed send"),
        Err(e) => log::warn!(
            "Failed to release the UTXO reservation for a failed send; these outputs stay \
             locked until the reservation expires and the next send, scan or init sweeps it: {e}"
        ),
    }
}

// Preparation can fail before its pending row ID is returned. Resolve it by the
// account-scoped idempotency key so this failure path also releases its inputs.
fn release_send_reservation_by_key(idempotency_key: &str, account_id: i64) {
    let released = get_db_connection()
        .map_err(|e| e.to_string())
        .and_then(|conn| {
            // Locks refer to pending_transactions.id, not its idempotency key.
            let pending = minotari_wallet::db::find_pending_transaction_record_by_idempotency_key(
                &conn,
                idempotency_key,
                account_id,
            )
            .map_err(|e| e.to_string())?;
            match pending {
                Some(record) => expire_and_unlock_pending_transaction(&conn, &record.id)
                    .map_err(|e| e.to_string()),
                None => Ok(false),
            }
        });

    match released {
        Ok(true) => log::info!("Released the UTXO reservation for a failed send"),
        // Already `Completed`/`Expired`: nothing to release, which is the safe outcome.
        Ok(false) => log::debug!("No pending UTXO reservation to release for a failed send"),
        Err(e) => log::warn!(
            "Failed to release the UTXO reservation for a failed send; these outputs stay \
             locked until the reservation expires and the next send, scan or init sweeps it: {e}"
        ),
    }
}

/// Check that `key_manager` (derived from the caller's seed words) controls the same
/// wallet as the stored account.
///
/// Compares public **view keys**, which is exactly the equality
/// `sign_locked_transaction`'s payload-signature check enforces later, so this accepts
/// precisely the inputs signing would accept — it cannot reject a send that would have
/// worked.
fn verify_seed_words_match_account(
    wallet_name: &str,
    passphrase: &str,
    key_manager: &KeyManager,
) -> Result<()> {
    let conn = get_db_connection()?;
    let accounts = get_accounts(&conn, Some(wallet_name))?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;

    let stored_view_key = account
        .decrypt_wallet_type(passphrase)
        .map_err(|e| WalletError::wallet(e.to_string()))?
        .get_public_view_key();

    if key_manager.get_view_key().pub_key != stored_view_key {
        return Err(WalletError::signing(SEED_WORDS_ACCOUNT_MISMATCH).into());
    }

    Ok(())
}

/// The Dart-visible explanation for a seed/account mismatch.
///
/// Upstream's own message for this condition says the payload "was tampered with in
/// transit or is corrupt", which for this bridge is almost always wrong and actively
/// harmful: the real cause is a wrong passphrase or the wrong wallet, and telling users
/// their transaction was tampered with invites false security reports. This is a **new**
/// `details` string carried by the existing `WalletError::Signing` variant — the
/// `#[error("Signing Error: {details}")]` format itself is unchanged.
pub(crate) const SEED_WORDS_ACCOUNT_MISMATCH: &str =
    "The supplied seed words do not match this wallet account. Check that the wallet name, \
     passphrase and seed words all belong to the same wallet.";

fn validate_confirmed_fee(actual_fee: u64, maximum: u64) -> Result<()> {
    if actual_fee > maximum {
        anyhow::bail!("Transaction fee exceeds the confirmed estimate. Review the payment again.");
    }
    Ok(())
}

/// Convert an upstream signing failure into a [`WalletError::Signing`].
///
/// The payload-integrity failure is re-worded to [`SEED_WORDS_ACCOUNT_MISMATCH`] for the
/// reason given there; every other signing failure keeps upstream's message verbatim.
fn map_signing_error(e: TransactionBuilderError) -> WalletError {
    let message = e.to_string();
    if message.contains("Offline payload integrity check failed") {
        WalletError::signing(SEED_WORDS_ACCOUNT_MISMATCH)
    } else {
        WalletError::signing(message)
    }
}

/// Build, sign, and broadcast a one-sided transaction, streaming progress.
///
/// Streams a [`SendTransactionEvent`] for each [`TransactionStage`] over `sink`
/// and resolves to the recorded, `Pending` [`DisplayedTransactionDto`]. `details`
/// carries the **secret** seed words/passphrase and the recipient/amount (in
/// microTari). Requires [`initialize_database`] first.
///
/// `Ok` means the send is recorded, not necessarily accepted by the node; the
/// scan's transaction monitor rebroadcasts it, so **do not resend after `Ok`**.
/// Rebroadcast only happens while `start_scan` runs.
/// `Network Error` only comes from failures before the send is recorded.
///
/// Async and streamed. The send **continues even if the Dart stream is closed**
/// (a half-broadcast transaction must finish) — this deliberately differs from
/// [`start_scan`](crate::api::scanner::start_scan), which cancels on a closed
/// sink. Errors propagate as the resolved `Err`.
#[frb]
pub async fn send_transaction(
    sink: StreamSink<SendTransactionEvent>,
    details: SendTransactionDetails,
) -> Result<DisplayedTransactionDto> {
    let stream_sink = sink.clone();

    send_transaction_with_handler(details, move |event| {
        // SEND deliberately ignores sink-closed: a half-broadcast transaction must
        // still finish even if the Dart UI stopped listening (aborting could lose
        // funds). This is the asymmetric counterpart to SCAN, which *cancels* on
        // sink-closed — see `scanner.rs::run_forwarder` for the full rationale.
        let _ = stream_sink.add(event);
    })
    .await
}

/// Send at an explicitly confirmed rate, refusing a fee above `max_fee` before
/// broadcasting and releasing the reservation on refusal. Rate is microTari per
/// gram; max_fee is microTari. Existing send API and DTO remain unchanged.
#[frb]
pub async fn send_transaction_with_fee(
    sink: StreamSink<SendTransactionEvent>,
    details: SendTransactionDetails,
    fee_per_gram: u64,
    max_fee: u64,
) -> Result<()> {
    let events = sink.clone();
    let result =
        send_transaction_with_fee_handler(details, Some((fee_per_gram, max_fee)), move |event| {
            let _ = events.add(event);
        })
        .await;
    if let Err(error) = result {
        // FRB runs this function on a separate unawaited normal-task channel.
        // Return errors on the stream Cake listens to before dropping its sink;
        // do not leave an unhandled error on the normal-task Future instead.
        let _ = sink.add_error(error);
    }
    Ok(())
}

/// Thin adapter: resolve the network (frozen `None → MainNet` default) and hand the
/// raw fields to the pure domain validator. `WalletError` is converted to the
/// boundary `anyhow::Error` via `?`.
fn validate_inputs(details: &SendTransactionDetails) -> Result<ValidatedInputs> {
    let network = parse_network(details.network);
    let validated = validate_send_inputs(
        network,
        &details.recipient_address,
        details.amount,
        details.confirmation_window,
    )?;
    Ok(validated)
}

/// Build the [`TransactionSender`] for `wallet_name`.
///
/// `passphrase` is borrowed from the caller's zeroizing container and copied once into
/// the `Zeroizing<String>` that `TransactionSender::new` takes ownership of, so the
/// plaintext exists only inside zeroizing containers for the whole call chain.
fn create_transaction_sender(
    wallet_name: &str,
    passphrase: &str,
    network: Network,
    confirmations: u64,
) -> Result<TransactionSender> {
    let db_pool = get_db_pool().map_err(|e| WalletError::database(e.to_string()))?;

    TransactionSender::new(
        db_pool,
        wallet_name.to_string(),
        Zeroizing::new(passphrase.to_string()),
        network,
        confirmations,
    )
    .map_err(|e| WalletError::wallet(e.to_string()).into())
}

/// Reserve inputs and build the unsigned transaction.
///
/// Returns the id of the pending row that now holds the `Locked` UTXOs alongside the
/// payload; the caller needs it to release them if a later step fails (see
/// [`release_send_reservation`]).
fn build_unsigned_transaction(
    sender: &mut TransactionSender,
    address: TariAddress,
    amount: MicroMinotari,
    payment_id: Option<String>,
) -> Result<(String, PrepareOneSidedTransactionForSigningResult)> {
    let recipient = Recipient {
        address,
        amount,
        payment_id,
    };

    let idempotency_key = uuid::Uuid::new_v4().to_string();

    let tx = match sender.start_new_transaction(
        idempotency_key.clone(),
        recipient,
        SECONDS_TO_LOCK_UTXO,
    ) {
        Ok(tx) => tx,
        Err(error) => {
            // Upstream can reserve inputs and then fail while preparing the
            // payload. The caller never receives the key on this error path.
            release_send_reservation_by_key(&idempotency_key, sender.account.id);
            return Err(
                WalletError::wallet(format!("Failed to build transaction: {}", error)).into(),
            );
        }
    };

    Ok((sender.processed_transactions.id().to_string(), tx))
}

#[cfg(test)]
mod tests {
    //! Boundary-adapter tests for `validate_inputs`. The pure validation logic now
    //! lives in `domain::validation` (tested there as `WalletError` variants); these
    //! tests pin the *boundary* behavior the adapter is responsible for: resolving
    //! the network via `parse_network` and converting the domain `WalletError` into
    //! the exact frozen Dart-visible `anyhow::Error` string. The known-good
    //! recipient address is derived deterministically from fixed key bytes (NOT a
    //! real funded address), so the suite stays hermetic and reproducible.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tari_common_types::types::CompressedPublicKey;
    use tari_crypto::ristretto::RistrettoSecretKey;
    use tari_utilities::ByteArray;

    #[test]
    fn confirmed_fee_is_an_upper_bound_including_uint64_edges() {
        assert!(validate_confirmed_fee(1, 1).is_ok());
        assert!(validate_confirmed_fee(1, u64::MAX).is_ok());
        assert!(validate_confirmed_fee(u64::MAX, u64::MAX).is_ok());
        assert!(validate_confirmed_fee(2, 1).is_err());
        assert!(validate_confirmed_fee(u64::MAX, 1).is_err());
    }

    /// Build a deterministic, valid base58 Tari (dual) address from fixed key
    /// bytes. The bytes are small little-endian scalars (well below the group
    /// order) so `RistrettoSecretKey::from_canonical_bytes` always succeeds. This
    /// is obviously not a real address — it exists only to exercise the base58
    /// parse path in `validate_inputs`.
    fn deterministic_recipient_base58(network: Network) -> String {
        let mut view_bytes = [0u8; 32];
        view_bytes[0] = 7;
        let mut spend_bytes = [0u8; 32];
        spend_bytes[0] = 11;

        let view_sk = RistrettoSecretKey::from_canonical_bytes(&view_bytes)
            .expect("fixed view-key bytes must be a canonical scalar");
        let spend_sk = RistrettoSecretKey::from_canonical_bytes(&spend_bytes)
            .expect("fixed spend-key bytes must be a canonical scalar");

        let view_pk = CompressedPublicKey::from_secret_key(&view_sk);
        let spend_pk = CompressedPublicKey::from_secret_key(&spend_sk);

        TariAddress::new_dual_address_with_default_features(view_pk, spend_pk, network)
            .expect("constructing a dual address from valid keys must succeed")
            .to_base58()
    }

    fn details_with(
        recipient: String,
        amount: u64,
        confirmation_window: Option<u64>,
    ) -> SendTransactionDetails {
        SendTransactionDetails {
            seed_words: Vec::new(),
            passphrase: None,
            network: Some(TariNetwork::MainNet),
            base_url: None,
            wallet_name: "test-wallet".to_string(),
            recipient_address: recipient,
            amount,
            payment_id: None,
            confirmation_window,
        }
    }

    #[test]
    fn rejects_zero_amount_with_frozen_boundary_string() {
        let details = details_with(deterministic_recipient_base58(Network::MainNet), 0, None);
        // `ValidatedInputs` deliberately has no `Debug`, so we destructure the
        // `Result` instead of using `expect_err`.
        let Err(err) = validate_inputs(&details) else {
            panic!("zero amount must be rejected");
        };
        // BASELINE CONTRACT: this exact Dart-visible string predates WalletError
        // (legacy `TransactionError::WalletError("Amount must be greater than zero")`).
        // The domain returns `WalletError::Wallet`; the adapter `?` converts it to
        // this anyhow string. Cake Wallet may match on it; it must not change.
        assert_eq!(
            err.to_string(),
            "Wallet Error: Amount must be greater than zero"
        );
    }

    #[test]
    fn rejects_malformed_recipient_address_with_frozen_boundary_prefix() {
        let details = details_with("not-a-valid-base58-address".to_string(), 1_000, None);
        let Err(err) = validate_inputs(&details) else {
            panic!("bad address must be rejected");
        };
        // BASELINE CONTRACT: the boundary keeps the legacy
        // `"Invalid Recipient Address: …"` prefix (was `TransactionError::InvalidAddress`).
        assert!(
            err.to_string().starts_with("Invalid Recipient Address: "),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn rejects_wrong_network_recipient_with_frozen_boundary_prefix() {
        // The wallet's details resolve to MainNet (`details_with` hard-codes it), but
        // the recipient address is encoded for Esmeralda — a cross-network send.
        let details = details_with(
            deterministic_recipient_base58(Network::Esmeralda),
            1_000,
            None,
        );
        let Err(err) = validate_inputs(&details) else {
            panic!("a cross-network recipient must be rejected");
        };
        // BASELINE CONTRACT: the new rejection reuses the frozen
        // `"Invalid Recipient Address: "` prefix (Shared Contracts §2).
        assert!(
            err.to_string().starts_with("Invalid Recipient Address: "),
            "unexpected message: {err}"
        );
        // Pin the exact new message. `Network`'s `Display` emits lowercase key strings
        // (`mainnet` / `esmeralda`), so the values are lowercase (ledger D2).
        assert_eq!(
            err.to_string(),
            "Invalid Recipient Address: address network (esmeralda) does not match the configured network (mainnet)"
        );
    }

    /// SEND-continues-on-sink-closed (contrast with SCAN-cancels). The send-side
    /// status callback has signature `Fn(SendTransactionEvent)` returning `()`, so a
    /// closed sink *structurally cannot* signal failure back into the transaction
    /// flow — the tx proceeds regardless. This pins that the wrapper closure
    /// `send_transaction` installs (a failing `sink.add` swallowed by `let _ = …`)
    /// keeps reporting every subsequent stage rather than aborting. We model it
    /// hermetically (no DB) by replaying the closure shape against a fake sink whose
    /// `add` always errors, and asserting all stages were still delivered in order.
    #[test]
    fn send_continues_reporting_when_sink_is_closed() {
        use std::sync::{Arc, Mutex};

        // A fake sink that always rejects (models a closed Dart StreamSink).
        let delivered: Arc<Mutex<Vec<TransactionStage>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_add = {
            let delivered = delivered.clone();
            move |event: SendTransactionEvent| -> Result<()> {
                delivered.lock().unwrap().push(event.stage);
                Err(WalletError::scan("sink closed").into())
            }
        };

        // The exact wrapper shape `send_transaction` uses: swallow the add error so
        // the status_callback (and therefore the surrounding tx flow) keeps going.
        let status_callback = move |event: SendTransactionEvent| {
            let _ = sink_add(event);
        };

        // Drive the full stage sequence through the callback as the real flow would.
        for stage in [
            TransactionStage::Initializing,
            TransactionStage::ValidatingInput,
            TransactionStage::Broadcasting,
            TransactionStage::Completed,
        ] {
            status_callback(SendTransactionEvent {
                stage,
                details: String::new(),
            });
        }

        // Despite every `add` failing, all stages were still reported in order: the
        // send path does not cancel on sink-closed.
        let got = delivered.lock().unwrap();
        assert_eq!(got.len(), 4, "all stages reported even with a closed sink");
        assert!(matches!(got[0], TransactionStage::Initializing));
        assert!(matches!(got[3], TransactionStage::Completed));
    }

    /// The secrets are *moved* out of the caller-owned `SendTransactionDetails`, not
    /// cloned, so no un-wiped duplicate of the seed words or passphrase is left behind
    /// in the struct. This mirrors the first statements of `send_transaction_with_handler`.
    #[test]
    fn secrets_are_moved_out_of_details_leaving_no_copy_behind() {
        let mut details = details_with(deterministic_recipient_base58(Network::MainNet), 1, None);
        details.seed_words = vec!["word-a".to_string(), "word-b".to_string()];
        details.passphrase = Some("a-passphrase".to_string());

        let seed_words = Zeroizing::new(std::mem::take(&mut details.seed_words));
        let passphrase = Zeroizing::new(
            details
                .passphrase
                .take()
                .unwrap_or_else(|| DEFAULT_PASSPHRASE.to_string()),
        );

        assert_eq!(
            seed_words.len(),
            2,
            "the seed words moved into the container"
        );
        assert_eq!(&*passphrase, "a-passphrase");
        assert!(
            details.seed_words.is_empty(),
            "no seed words may remain in the caller-owned struct"
        );
        assert!(
            details.passphrase.is_none(),
            "no passphrase may remain in the caller-owned struct"
        );
    }

    /// Characterization test for the seed/account mismatch message.
    ///
    /// Pins two things: that the mismatch is reported through the **existing**
    /// `Signing Error: ` prefix (the frozen `#[error]` format is untouched), and that
    /// the `details` we substitute does not repeat upstream's "tampered with in
    /// transit" wording, which would push users towards false security reports.
    #[test]
    fn seed_account_mismatch_does_not_accuse_the_user_of_tampering() {
        let rendered = WalletError::signing(SEED_WORDS_ACCOUNT_MISMATCH).to_string();

        assert!(
            rendered.starts_with("Signing Error: "),
            "the frozen Signing error format must be unchanged, got {rendered:?}"
        );
        assert!(
            rendered.contains("do not match this wallet account"),
            "the message must name the real cause, got {rendered:?}"
        );
        for forbidden in ["tampered", "corrupt", "integrity"] {
            assert!(
                !rendered.to_lowercase().contains(forbidden),
                "the message must not imply tampering (found {forbidden:?}) in {rendered:?}"
            );
        }
    }

    /// `map_signing_error` re-words only the payload-integrity failure and passes every
    /// other upstream signing error through verbatim.
    #[test]
    fn map_signing_error_rewords_only_the_integrity_failure() {
        let integrity = TransactionBuilderError::Other(
            "Offline payload integrity check failed: payload signature is invalid. The payload \
             was tampered with in transit or is corrupt."
                .to_string(),
        );
        assert_eq!(
            map_signing_error(integrity).to_string(),
            WalletError::signing(SEED_WORDS_ACCOUNT_MISMATCH).to_string(),
            "the integrity failure must be re-worded"
        );

        let other = TransactionBuilderError::Other("some other signing failure".to_string());
        let rendered = map_signing_error(other).to_string();
        assert!(
            rendered.contains("some other signing failure"),
            "unrelated signing errors must pass through verbatim, got {rendered:?}"
        );
    }

    /// A temp wallet DB (no globals) holding one spendable output that
    /// `build_unsigned_transaction` has reserved. Seeded the way upstream's own tests do.
    /// Returns the temp dir (keep it alive), the pool, the output id and the pending row id.
    fn reserve_one_output() -> (
        tempfile::TempDir,
        minotari_wallet::db::SqlitePool,
        i64,
        String,
    ) {
        use minotari_wallet::db::{
            create_account, get_account_by_name, init_db, insert_output, insert_scanned_tip_block,
            mark_output_confirmed,
        };
        use tari_common_types::seeds::cipher_seed::CipherSeed;
        use tari_common_types::types::{FixedHash, PrivateKey};
        use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
        use tari_transaction_components::transaction_components::{MemoField, WalletOutput};

        let dir = tempfile::tempdir().unwrap();
        let pool = init_db(dir.path().join("wallet.db")).unwrap();
        let conn = pool.get().unwrap();

        let wallet =
            WalletType::SeedWords(SeedWordsWallet::construct_new(CipherSeed::random()).unwrap());
        create_account(&conn, "test", &wallet, "pass").unwrap();
        let account = get_account_by_name(&conn, "test").unwrap().unwrap();
        insert_scanned_tip_block(&conn, account.id, 200, &[0u8; 32]).unwrap();

        // One spendable output, mined and confirmed at 100 under a tip of 200.
        let output = WalletOutput::new_from_parts(
            Default::default(),
            MicroMinotari(1_000_000),
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
            MemoField::new_empty(),
            FixedHash::from([1u8; 32]),
            Default::default(),
        );
        let output_id = insert_output(
            &conn,
            account.id,
            &PrivateKey::default(),
            vec![1u8; 32],
            &output,
            100,
            &FixedHash::zero(),
            1_700_000_000,
            None,
            None,
            Default::default(),
            false,
        )
        .unwrap();
        mark_output_confirmed(&conn, &FixedHash::from([1u8; 32]), 100, &[0u8; 32]).unwrap();

        let address = account.get_address(Network::LocalNet, "pass").unwrap();
        let mut sender = TransactionSender::new(
            pool.clone(),
            "test".to_string(),
            Zeroizing::new("pass".to_string()),
            Network::LocalNet,
            3,
        )
        .unwrap();
        let (pending_id, _) =
            build_unsigned_transaction(&mut sender, address, MicroMinotari(1_000), None).unwrap();

        drop(conn);
        (dir, pool, output_id, pending_id)
    }

    fn output_status(conn: &r2d2_sqlite::rusqlite::Connection, output_id: i64) -> String {
        conn.query_row(
            "SELECT status FROM outputs WHERE id = ?1",
            [output_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Exception to the DB-free unit-test rule: a reservation released by the wrong id
    /// leaves the user's UTXOs locked with no error, so this pins the id seam against a
    /// real temp DB.
    #[test]
    fn reservation_is_released_by_pending_row_id_not_idempotency_key() {
        use minotari_wallet::db::get_output_lock_request_id;

        let (_dir, pool, output_id, pending_id) = reserve_one_output();
        let conn = pool.get().unwrap();

        assert_eq!(output_status(&conn, output_id), "LOCKED");
        assert_eq!(
            get_output_lock_request_id(&conn, output_id).unwrap(),
            Some(pending_id.clone()),
            "the returned id must be the pending row that holds the lock"
        );

        // The idempotency key is a different value and releases nothing.
        let idempotency_key: String = conn
            .query_row(
                "SELECT idempotency_key FROM pending_transactions WHERE id = ?1",
                [&pending_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(idempotency_key, pending_id);
        assert!(!expire_and_unlock_pending_transaction(&conn, &idempotency_key).unwrap());
        assert_eq!(output_status(&conn, output_id), "LOCKED");

        // The pending row id releases the output.
        assert!(expire_and_unlock_pending_transaction(&conn, &pending_id).unwrap());
        assert_eq!(output_status(&conn, output_id), "UNSPENT");
        assert_eq!(get_output_lock_request_id(&conn, output_id).unwrap(), None);
    }

    /// Exception to the DB-free unit-test rule (see above): the sweep run at database
    /// init and at send/scan start releases a stale reservation but not a live one.
    #[test]
    fn expired_reservations_are_released_on_database_init() {
        let (_dir, pool, output_id, pending_id) = reserve_one_output();
        let conn = pool.get().unwrap();

        // Not expired yet: the sweep leaves it alone.
        crate::api::db::release_expired_reservations(&conn);
        assert_eq!(output_status(&conn, output_id), "LOCKED");

        // A reservation stranded long ago, e.g. by an app kill before recording.
        conn.execute(
            "UPDATE pending_transactions SET expires_at = '2000-01-01 00:00:00' WHERE id = ?1",
            [&pending_id],
        )
        .unwrap();
        crate::api::db::release_expired_reservations(&conn);
        assert_eq!(output_status(&conn, output_id), "UNSPENT");
    }

    #[test]
    fn expiry_sweep_never_releases_inputs_of_a_recorded_send() {
        let (_dir, pool, output_id, pending_id) = reserve_one_output();
        let conn = pool.get().unwrap();

        // Recording a send completes its reservation, even while the displayed
        // transaction is Pending node acceptance. Those inputs cannot be reused.
        conn.execute(
            "UPDATE pending_transactions SET status = 'COMPLETED', \
             expires_at = '2000-01-01 00:00:00' WHERE id = ?1",
            [&pending_id],
        )
        .unwrap();

        crate::api::db::release_expired_reservations(&conn);
        assert_eq!(output_status(&conn, output_id), "LOCKED");
        assert!(!expire_and_unlock_pending_transaction(&conn, &pending_id).unwrap());
        assert_eq!(output_status(&conn, output_id), "LOCKED");
    }

    #[test]
    fn adapter_resolves_network_and_passes_through_valid_inputs() {
        let details = details_with(
            deterministic_recipient_base58(Network::MainNet),
            1_000,
            None,
        );
        let validated = validate_inputs(&details).expect("valid inputs must succeed");
        assert_eq!(validated.amount, MicroMinotari(1_000));
        // Proves the adapter resolved `Some(MainNet)` via `parse_network`.
        assert_eq!(validated.network, Network::MainNet);
    }
}
