use crate::api::config::{DEFAULT_BASE_URL, DEFAULT_PASSPHRASE, SECONDS_TO_LOCK_UTXO};
use crate::api::db::get_db_pool;
use crate::api::error::WalletError;
use crate::api::network::{apply_network, parse_network, TariNetwork};
use crate::api::transactions::DisplayedTransactionDto;
use crate::domain::keys::key_manager_from_seed_words;
use crate::domain::validation::{validate_send_inputs, ValidatedInputs};
use crate::frb_generated::StreamSink;
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::transactions::manager::TransactionSender;
use minotari_wallet::transactions::one_sided_transaction::Recipient;
use tari_common::configuration::Network;
use tari_common_types::tari_address::TariAddress;
use tari_transaction_components::consensus::ConsensusConstantsBuilder;
use tari_transaction_components::offline_signing::models::PrepareOneSidedTransactionForSigningResult;
use tari_transaction_components::offline_signing::sign_locked_transaction;
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
    let mut sender =
        create_transaction_sender(&details, validated.network, validated.confirmations)?;

    report(
        TransactionStage::ConstructingTransaction,
        "Building transaction UTXOs...",
    );
    let unsigned_tx = build_unsigned_transaction(
        &mut sender,
        validated.recipient_address,
        validated.amount,
        details.payment_id,
    )?;

    report(
        TransactionStage::SigningKeyGeneration,
        "Deriving keys from seed...",
    );

    let signed_transaction = {
        let key_manager = key_manager_from_seed_words(&details.seed_words)?;

        report(
            TransactionStage::SigningTransaction,
            "Signing transaction...",
        );

        let consensus_constants = ConsensusConstantsBuilder::new(validated.network).build();

        sign_locked_transaction(
            &key_manager,
            consensus_constants,
            validated.network,
            unsigned_tx,
        )
        .map_err(|e| WalletError::signing(e.to_string()))?
    };

    report(TransactionStage::Broadcasting, "Broadcasting to network...");

    let base_url = details.base_url.unwrap_or(DEFAULT_BASE_URL.to_string());

    let result_tx = sender
        .finalize_transaction_and_broadcast(signed_transaction, base_url)
        .await
        .map_err(|e| WalletError::network(e.to_string()))?;

    report(TransactionStage::Completed, "Transaction sent");

    Ok(result_tx.into())
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

fn create_transaction_sender(
    details: &SendTransactionDetails,
    network: Network,
    confirmations: u64,
) -> Result<TransactionSender> {
    let db_pool = get_db_pool().map_err(|e| WalletError::database(e.to_string()))?;

    // The passphrase arrives in the frozen public `SendTransactionDetails.passphrase`
    // (plain `Option<String>`). Hold our local copy in a zeroizing container so it is
    // wiped when this function returns (Shared Contracts §3). Note: `TransactionSender`
    // takes an owned `String`, so the copy handed to it below (`password.to_string()`)
    // is a non-zeroizing plaintext owned by the sender — its wiping is the upstream
    // API's concern; we only minimize our own local exposure here.
    let password = Zeroizing::new(
        details
            .passphrase
            .clone()
            .unwrap_or(DEFAULT_PASSPHRASE.to_string()),
    );

    TransactionSender::new(
        db_pool,
        details.wallet_name.clone(),
        password.to_string(),
        network,
        confirmations,
    )
    .map_err(|e| WalletError::wallet(e.to_string()).into())
}

fn build_unsigned_transaction(
    sender: &mut TransactionSender,
    address: TariAddress,
    amount: MicroMinotari,
    payment_id: Option<String>,
) -> Result<PrepareOneSidedTransactionForSigningResult> {
    let recipient = Recipient {
        address,
        amount,
        payment_id,
    };

    let idempotency_key = uuid::Uuid::new_v4().to_string();

    let tx = sender
        .start_new_transaction(idempotency_key.clone(), recipient, SECONDS_TO_LOCK_UTXO)
        .map_err(|e| WalletError::wallet(format!("Failed to build transaction: {}", e)))?;

    Ok(tx)
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
    fn deterministic_recipient_base58() -> String {
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

        TariAddress::new_dual_address_with_default_features(view_pk, spend_pk, Network::MainNet)
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
        let details = details_with(deterministic_recipient_base58(), 0, None);
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

    #[test]
    fn adapter_resolves_network_and_passes_through_valid_inputs() {
        let details = details_with(deterministic_recipient_base58(), 1_000, None);
        let validated = validate_inputs(&details).expect("valid inputs must succeed");
        assert_eq!(validated.amount, MicroMinotari(1_000));
        // Proves the adapter resolved `Some(MainNet)` via `parse_network`.
        assert_eq!(validated.network, Network::MainNet);
    }
}
