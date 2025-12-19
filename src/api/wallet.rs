use flutter_rust_bridge::frb;
use std::str::FromStr;
use minotari_wallet::init_with_view_key;
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
use crate::api::db::DB_PATH;

#[frb]
pub struct WalletCreationDetails {
    pub tari_address: String,
    pub wallet_birthday: u16,
    pub spend_public_key_hex: String,
    pub view_private_key_hex: String,
}

#[frb]
pub async fn create_wallet() -> Result<WalletCreationDetails, String> {
    let tari_cipher_seed = CipherSeed::random();
    let wallet_birthday = tari_cipher_seed.birthday();
    let seed_words_wallet = SeedWordsWallet::construct_new(tari_cipher_seed).map_err(|_| "Invalid seeds")?;
    let wallet = WalletType::SeedWords(seed_words_wallet);
    let key_manager = KeyManager::new(wallet).map_err(|_| "Invalid key manager")?;

    let view_key = key_manager.get_private_view_key();
    let spend_key = key_manager.get_spend_key();

    let public_view_key = CompressedKey::from_secret_key(&view_key);

    let tari_address = TariAddress::new_dual_address(
        public_view_key,
        spend_key.pub_key.clone(),
        Network::MainNet,
        TariAddressFeatures::create_one_sided_only(),
        None,
    ).map_err(|_| "Invalid address")?;

    let wcd = WalletCreationDetails {
        tari_address: tari_address.to_base58(),
        wallet_birthday,
        spend_public_key_hex: spend_key.pub_key.to_hex(),
        view_private_key_hex: view_key.to_hex(),
    };

    let db = DB_PATH.get().expect("couldn't read db path");
    init_with_view_key(&wcd.view_private_key_hex, &wcd.spend_public_key_hex, "", db, 0, None).await.map_err(|e| e.to_string())?;

    Ok(wcd)
}


#[frb]
pub async fn restore_wallet(seed_words: Vec<String>, passphrase: Option<String>) -> Result<WalletCreationDetails, String> {
    let seeds = SeedWords::from_str(&seed_words.join(" ")).map_err(|_| "Invalid seeds")?;
    let password = passphrase.map(|d| SafePassword::from_str(&d).expect("Invalid password"));
    let tari_cipher_seed = CipherSeed::from_mnemonic(&seeds, password).expect("Invalid cipher");
    let wallet_birthday = tari_cipher_seed.birthday();
    let seed_words_wallet = SeedWordsWallet::construct_new(tari_cipher_seed).map_err(|_| "Invalid seeds")?;
    let wallet = WalletType::SeedWords(seed_words_wallet);
    let key_manager = KeyManager::new(wallet).map_err(|_| "Invalid key manager")?;

    let view_key = key_manager.get_private_view_key();
    let spend_key = key_manager.get_spend_key();

    let public_view_key = CompressedKey::from_secret_key(&view_key);

    let tari_address = TariAddress::new_dual_address(
        public_view_key,
        spend_key.pub_key.clone(),
        Network::MainNet,
        TariAddressFeatures::create_one_sided_only(),
        None,
    ).map_err(|_| "Invalid address")?;

    Ok(WalletCreationDetails {
        tari_address: tari_address.to_base58(),
        wallet_birthday,
        spend_public_key_hex: spend_key.pub_key.to_hex(),
        view_private_key_hex: view_key.to_hex(),
    })
}
