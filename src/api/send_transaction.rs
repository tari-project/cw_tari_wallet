use crate::api::db::get_db_pool;
use crate::api::network::{parse_network, TariNetwork};
use crate::api::transactions::DisplayedTransactionDto;
use crate::frb_generated::StreamSink;
use anyhow::{anyhow, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::transactions::manager::TransactionSender;
use minotari_wallet::transactions::one_sided_transaction::Recipient;
use std::str::FromStr;
use tari_common::configuration::Network;
use tari_common::network_check::set_network_if_choice_valid;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::Mnemonic;
use tari_common_types::seeds::seed_words::SeedWords;
use tari_common_types::tari_address::TariAddress;
use tari_transaction_components::consensus::ConsensusConstantsBuilder;
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_transaction_components::key_manager::KeyManager;
use tari_transaction_components::offline_signing::models::PrepareOneSidedTransactionForSigningResult;
use tari_transaction_components::offline_signing::sign_locked_transaction;
use tari_transaction_components::MicroMinotari;
use thiserror::Error;

const DEFAULT_BASE_URL: &str = "https://rpc.tari.com";
const DEFAULT_PASSPHRASE: &str = "";
const DEFAULT_WALLET_NAME: &str = "default";
const DEFAULT_CONFIRMATION_WINDOW: u64 = 3;
const SECONDS_TO_LOCK_UTXO: u64 = 60 * 60 * 24; // 24 hrs

#[frb]
pub struct SendTransactionDetails {
    pub seed_words: Vec<String>,
    pub passphrase: Option<String>,
    pub network: Option<TariNetwork>,
    pub base_url: Option<String>,
    pub wallet_name: Option<String>,
    pub recipient_address: String,
    pub amount: u64,
    pub payment_id: Option<String>,
    pub confirmation_window: Option<u64>,
}

#[frb]
#[derive(Clone, Debug)]
pub enum TransactionStage {
    Initializing,
    ValidatingInput,
    ConnectingToNetwork,
    FetchingBalance,
    ConstructingTransaction,
    SigningKeyGeneration,
    SigningTransaction,
    Broadcasting,
    Completed,
}

#[frb]
#[derive(Clone)]
pub struct SendTransactionEvent {
    pub stage: TransactionStage,
    pub details: String,
}

#[derive(Error, Debug)]
pub enum TransactionError {
    #[error("Invalid Recipient Address: {0}")]
    InvalidAddress(String),

    #[error("Invalid Seed Words: {0}")]
    InvalidSeedWords(String),

    #[error("Invalid Passphrase")]
    InvalidPassphrase,

    #[error("Wallet Error: {0}")]
    WalletError(String),

    #[error("Network Error: {0}")]
    NetworkError(String),

    #[error("Database Error: {0}")]
    DatabaseError(String),

    #[error("Signing Error: {0}")]
    SigningError(String),

    #[error("Aborted by User")]
    Aborted,
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

    set_network_if_choice_valid(validated.network)?;

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
        let key_manager = derive_key_manager(&details.seed_words)?;

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
        .map_err(|e| TransactionError::SigningError(e.to_string()))?
    };

    report(TransactionStage::Broadcasting, "Broadcasting to network...");

    let base_url = details.base_url.unwrap_or(DEFAULT_BASE_URL.to_string());

    let result_tx = sender
        .finalize_transaction_and_broadcast(signed_transaction, base_url)
        .await
        .map_err(|e| TransactionError::NetworkError(e.to_string()))?;

    report(TransactionStage::Completed, "Transaction sent");

    Ok(result_tx.into())
}

#[frb]
pub async fn send_transaction(
    sink: StreamSink<SendTransactionEvent>,
    details: SendTransactionDetails,
) -> Result<DisplayedTransactionDto> {
    let stream_sink = sink.clone();

    send_transaction_with_handler(details, move |event| {
        // We ignore errors here because if the sink is closed,
        // we likely still want the transaction to finish processing if possible,
        let _ = stream_sink.add(event);
    })
    .await
}

struct ValidatedInputs {
    network: Network,
    recipient_address: TariAddress,
    amount: MicroMinotari,
    confirmations: u64,
}

fn validate_inputs(details: &SendTransactionDetails) -> Result<ValidatedInputs> {
    let network = parse_network(details.network);

    let recipient_address = TariAddress::from_base58(&details.recipient_address)
        .map_err(|e| TransactionError::InvalidAddress(e.to_string()))?;

    if details.amount == 0 {
        return Err(anyhow!(TransactionError::WalletError(
            "Amount must be greater than zero".to_string()
        )));
    }

    Ok(ValidatedInputs {
        network,
        recipient_address,
        amount: MicroMinotari(details.amount),
        confirmations: details
            .confirmation_window
            .unwrap_or(DEFAULT_CONFIRMATION_WINDOW),
    })
}

fn create_transaction_sender(
    details: &SendTransactionDetails,
    network: Network,
    confirmations: u64,
) -> Result<TransactionSender> {
    let db_pool = get_db_pool().map_err(|e| TransactionError::DatabaseError(e.to_string()))?;

    let password = details
        .passphrase
        .clone()
        .unwrap_or(DEFAULT_PASSPHRASE.to_string());

    let wallet_name = details
        .wallet_name
        .clone()
        .unwrap_or(DEFAULT_WALLET_NAME.to_string());

    TransactionSender::new(db_pool, wallet_name, password, network, confirmations)
        .map_err(|e| TransactionError::WalletError(e.to_string()).into())
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
        .map_err(|e| {
            TransactionError::WalletError(format!("Failed to build transaction: {}", e))
        })?;

    Ok(tx)
}

fn derive_key_manager(seed_words: &[String]) -> Result<KeyManager> {
    let seed_str = seed_words.join(" ");
    let mnemonic = SeedWords::from_str(&seed_str)
        .map_err(|e| TransactionError::InvalidSeedWords(e.to_string()))?;

    let seed = CipherSeed::from_mnemonic(&mnemonic, None)
        .map_err(|e| TransactionError::InvalidSeedWords(format!("Cipher Seed error: {}", e)))?;

    let wallet_type = WalletType::SeedWords(SeedWordsWallet::construct_new(seed).map_err(|e| {
        TransactionError::WalletError(format!("Wallet construction failed: {}", e))
    })?);

    KeyManager::new(wallet_type)
        .map_err(|e| TransactionError::WalletError(format!("Key Manager failed: {}", e)).into())
}
