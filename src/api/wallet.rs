use crate::api::db::DB_PATH;
use flutter_rust_bridge::frb;
use minotari_wallet::init_with_view_key;
use std::str::FromStr;
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::Mnemonic;
use tari_common_types::seeds::seed_words::SeedWords;
use tari_common_types::tari_address::{TariAddress, TariAddressFeatures};
use tari_crypto::compressed_key::CompressedKey;
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_transaction_components::key_manager::{KeyManager, TransactionKeyManagerInterface};
use tari_utilities::hex::Hex;
use tari_utilities::SafePassword;

#[frb]
pub struct WalletCreationDetails {
    pub tari_address: String,
    pub wallet_birthday: u16,
    pub spend_public_key_hex: String,
    pub view_private_key_hex: String,
}

#[frb]
pub async fn create_wallet(network: Option<String>) -> Result<WalletCreationDetails, String> {
    let network = parse_network(network)?;
    let seed = CipherSeed::random();

    let details = generate_details_from_seed(seed, network)?;
    initialize_wallet(&details)?;

    Ok(details)
}

#[frb]
pub async fn restore_wallet(
    seed_words: Vec<String>,
    passphrase: Option<String>,
    network: Option<String>,
) -> Result<WalletCreationDetails, String> {
    let network = parse_network(network)?;
    let mnemonic = SeedWords::from_str(&seed_words.join(" ")).map_err(|_| "Invalid seed words")?;
    let password = passphrase
        .map(|p| SafePassword::from_str(&p))
        .transpose()
        .map_err(|_| "Invalid password")?;

    let seed = CipherSeed::from_mnemonic(&mnemonic, password).map_err(|_| "Invalid cipher seed")?;

    let details = generate_details_from_seed(seed, network)?;
    initialize_wallet(&details)?;

    Ok(details)
}

fn parse_network(network: Option<String>) -> Result<Network, String> {
    network
        .as_deref()
        .map_or_else(|| Ok(Network::MainNet), Network::from_str)
        .map_err(|err| err.to_string())
}

fn generate_details_from_seed(
    seed: CipherSeed,
    network: Network,
) -> Result<WalletCreationDetails, String> {
    let wallet_birthday = seed.birthday();
    let wallet_type = WalletType::SeedWords(
        SeedWordsWallet::construct_new(seed).map_err(|_| "Failed to construct wallet from seed")?,
    );
    let key_manager = KeyManager::new(wallet_type).map_err(|_| "Failed to create key manager")?;

    let view_key = key_manager.get_private_view_key();
    let spend_key = key_manager.get_spend_key();
    let public_view_key = CompressedKey::from_secret_key(&view_key);

    let tari_address = TariAddress::new_dual_address(
        public_view_key,
        spend_key.pub_key.clone(),
        network,
        TariAddressFeatures::create_one_sided_only(),
        None,
    )
    .map_err(|_| "Failed to generate Tari address")?;

    Ok(WalletCreationDetails {
        tari_address: tari_address.to_base58(),
        wallet_birthday,
        spend_public_key_hex: spend_key.pub_key.to_hex(),
        view_private_key_hex: view_key.to_hex(),
    })
}

fn initialize_wallet(details: &WalletCreationDetails) -> Result<(), String> {
    let db = DB_PATH.get().ok_or("Database path not initialized")?;
    init_with_view_key(
        &details.view_private_key_hex,
        &details.spend_public_key_hex,
        "",
        db,
        0,
        None,
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}
