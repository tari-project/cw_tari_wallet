use crate::api::config::{DEFAULT_BASE_URL, DEFAULT_PASSPHRASE, SECONDS_TO_LOCK_UTXO};
use crate::api::db::{get_db_connection, get_db_pool};
use crate::api::error::WalletError;
use crate::api::network::{apply_network, parse_network, TariNetwork};
use crate::api::transactions::DisplayedTransactionDto;
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
    mut details: SendTransactionDetails,
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

    // Derive the signing key manager and prove it belongs to this account *before*
    // `start_new_transaction` reserves any UTXO. Since the bump, `sign_locked_transaction`
    // verifies a payload-integrity signature made with the preparing wallet's view key,
    // so a seed/account mismatch fails at signing — by which point the inputs are already
    // `Locked`, and nothing in this embedding ever unlocks them (see
    // `release_send_reservation`). Checking first makes that mismatch cost nothing.
    //
    // No stage event is emitted here: the streamed `TransactionStage` sequence is frozen
    // contract, so `SigningKeyGeneration` stays at its original position below.
    let key_manager = key_manager_from_seed_words(&seed_words)?;
    verify_seed_words_match_account(&details.wallet_name, &passphrase, &key_manager)?;

    report(
        TransactionStage::ConstructingTransaction,
        "Building transaction UTXOs...",
    );
    // From here on UTXOs are reserved, so every failure path must release them.
    let (idempotency_key, unsigned_tx) = build_unsigned_transaction(
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
            release_send_reservation(&idempotency_key);
            return Err(map_signing_error(e).into());
        }
    };

    report(TransactionStage::Broadcasting, "Broadcasting to network...");

    let base_url = details
        .base_url
        .take()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let result_tx = match sender
        .finalize_transaction_and_broadcast(signed_transaction, base_url)
        .await
    {
        Ok(tx) => tx,
        Err(e) => {
            release_send_reservation(&idempotency_key);
            return Err(WalletError::network(e.to_string()).into());
        }
    };

    report(TransactionStage::Completed, "Transaction sent");

    Ok(result_tx.into())
}

/// Release the UTXOs this send reserved, best-effort.
///
/// `start_new_transaction` flips the selected outputs to `Locked` and commits. Upstream
/// releases them again from `TransactionUnlocker::unlock_expired_transactions`, but that
/// only ever runs from `minotari`'s **daemon**, which Cake Wallet does not link — it
/// links this library. `fetch_unspent_outputs` has no expiry clause either, so nothing
/// re-examines a stale lock: without this call `SECONDS_TO_LOCK_UTXO` is decorative and
/// the lock is effectively permanent.
///
/// `expire_and_unlock_pending_transaction` only acts while the row is still `Pending`, so
/// it cannot hand back the inputs of a transaction that already reached the network.
///
/// Best-effort by design: this runs on an error path that already has a cause worth
/// reporting, so a failure here is logged rather than allowed to mask the original error.
fn release_send_reservation(idempotency_key: &str) {
    let released = get_db_connection()
        .map_err(|e| e.to_string())
        .and_then(|conn| {
            expire_and_unlock_pending_transaction(&conn, idempotency_key).map_err(|e| e.to_string())
        });

    match released {
        Ok(true) => log::info!("Released the UTXO reservation for a failed send"),
        // Already `Completed`/`Expired`: nothing to release, which is the safe outcome.
        Ok(false) => log::debug!("No pending UTXO reservation to release for a failed send"),
        Err(e) => log::warn!(
            "Failed to release the UTXO reservation for a failed send; these outputs stay \
             locked until the account is rescanned: {e}"
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
/// and resolves to the broadcast [`DisplayedTransactionDto`]. `details` carries
/// the **secret** seed words/passphrase and the recipient/amount (in microTari).
/// Requires [`initialize_database`] first.
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
/// Returns the idempotency key alongside the payload: it identifies the pending row that
/// now holds the `Locked` UTXOs, and the caller needs it to release them if any later
/// step fails (see [`release_send_reservation`]).
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

    let tx = sender
        .start_new_transaction(idempotency_key.clone(), recipient, SECONDS_TO_LOCK_UTXO)
        .map_err(|e| WalletError::wallet(format!("Failed to build transaction: {}", e)))?;

    Ok((idempotency_key, tx))
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
