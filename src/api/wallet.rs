use crate::api::db::{get_db_connection, get_db_path};
use crate::api::network::{parse_network, TariNetwork};
use anyhow::{anyhow, Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::get_accounts;
use minotari_wallet::utils::init_wallet::{init_with_seed_words, init_with_view_key};
use std::str::FromStr;
use tari_common::configuration::Network;
use tari_common::network_check::set_network_if_choice_valid;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::{Mnemonic, MnemonicLanguage};
use tari_common_types::seeds::seed_words::SeedWords;
use tari_common_types::tari_address::{TariAddress, TariAddressFeatures};
use tari_common_types::types::{CompressedPublicKey, PrivateKey};
use tari_crypto::compressed_key::CompressedKey;
use tari_transaction_components::key_manager::wallet_types::{SeedWordsWallet, WalletType};
use tari_transaction_components::key_manager::{KeyManager, TransactionKeyManagerInterface};
use tari_utilities::hex::Hex;
use tari_utilities::hidden::Hidden;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

#[frb(opaque)]
#[derive(Clone, Debug, Zeroize, ZeroizeOnDrop)]
pub struct SensitiveSeeds {
    pub words: Vec<String>,
}

#[frb]
pub struct WalletCreationDetails {
    pub tari_address: String,
    pub wallet_birthday: u16,
    pub spend_public_key_hex: String,
    pub view_private_key_hex: String,
    pub seed_words: Option<SensitiveSeeds>,
}

#[frb]
pub fn create_wallet(
    wallet_name: String,
    network: Option<TariNetwork>,
    passphrase: String,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);

    let network = parse_network(network);
    set_network_if_choice_valid(network)?;
    let seed = CipherSeed::random();
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, Some(&wallet_name))
        .context("Failed to init wallet")?;

    Ok(details)
}

#[frb]
pub fn restore_wallet(
    wallet_name: String,
    seed_words: Vec<String>,
    passphrase: String,
    network: Option<TariNetwork>,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);
    let seed_words = Zeroizing::new(seed_words);

    let network = parse_network(network);
    set_network_if_choice_valid(network)?;

    let mnemonic_string = Zeroizing::new(seed_words.join(" "));
    let mnemonic = SeedWords::from_str(&mnemonic_string).context("Invalid seed words")?;
    let seed = CipherSeed::from_mnemonic(&mnemonic, None).context("Invalid cipher seed")?;
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, Some(&wallet_name))
        .context("Failed to init wallet")?;

    Ok(details)
}

#[frb]
pub fn import_view_only_wallet(
    wallet_name: String,
    view_private_key_hex: String,
    spend_public_key_hex: String,
    birthday: u16,
    passphrase: String,
    network: Option<TariNetwork>,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);
    let network_enum = parse_network(network);
    set_network_if_choice_valid(network_enum)?;

    let db_path = get_db_path()?;

    init_with_view_key(
        &view_private_key_hex,
        &spend_public_key_hex,
        &password,
        &db_path,
        birthday,
        Some(&wallet_name),
    )
    .context("Failed to init view-only wallet")?;

    let view_key = PrivateKey::from_hex(&view_private_key_hex)
        .map_err(|_| anyhow!("Invalid hex for view key"))?;
    let spend_key = CompressedPublicKey::from_hex(&spend_public_key_hex)
        .map_err(|_| anyhow!("Invalid hex for spend key"))?;

    construct_wallet_details(spend_key, view_key, birthday, network_enum, None)
}

#[frb]
pub fn get_seed_words(password: String) -> Result<SensitiveSeeds> {
    let password = Zeroizing::new(password);

    let conn = get_db_connection()?;
    let accounts = get_accounts(&conn, None)?;
    let account = accounts
        .first()
        .context("No accounts found for this wallet")?;

    let seed_words_obj = account
        .get_seed_words(&password)?
        .ok_or_else(|| anyhow!("Account does not have seed words"))?;

    let hidden_joined = Hidden::from(seed_words_obj.join(" "));
    let raw_words = split_hidden_words(hidden_joined);

    Ok(SensitiveSeeds { words: raw_words })
}

#[frb]
pub fn reveal_seed_words(handle: &SensitiveSeeds) -> Vec<String> {
    handle.words.clone()
}

fn generate_details_from_seed(
    seed: &CipherSeed,
    network: Network,
) -> Result<WalletCreationDetails> {
    let wallet_birthday = seed.birthday();
    let wallet_type = WalletType::SeedWords(
        SeedWordsWallet::construct_new(seed.clone())
            .map_err(|_| anyhow!("Failed to construct wallet from seed"))?,
    );
    let key_manager = KeyManager::new(wallet_type).context("Failed to create key manager")?;

    let view_key = key_manager.get_private_view_key();
    let spend_key = key_manager.get_spend_key();

    let mnemonic = seed.to_mnemonic(MnemonicLanguage::English, None)?;
    let raw_words = split_hidden_words(mnemonic.join(" "));
    let sensitive_seeds = SensitiveSeeds { words: raw_words };

    construct_wallet_details(
        spend_key.pub_key,
        view_key,
        wallet_birthday,
        network,
        Some(sensitive_seeds),
    )
}

fn construct_wallet_details(
    public_spend_key: CompressedPublicKey,
    private_view_key: PrivateKey,
    birthday: u16,
    network: Network,
    seed_words: Option<SensitiveSeeds>,
) -> Result<WalletCreationDetails> {
    let public_view_key = CompressedKey::from_secret_key(&private_view_key);

    let tari_address = TariAddress::new_dual_address(
        public_view_key,
        public_spend_key.clone(),
        network,
        TariAddressFeatures::create_one_sided_only(),
        None,
    )
    .context("Failed to generate Tari address")?;

    Ok(WalletCreationDetails {
        tari_address: tari_address.to_base58(),
        wallet_birthday: birthday,
        spend_public_key_hex: public_spend_key.to_hex(),
        view_private_key_hex: private_view_key.to_hex(),
        seed_words,
    })
}

fn split_hidden_words(hidden: Hidden<String>) -> Vec<String> {
    hidden
        .reveal()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

#[frb]
pub fn delete_wallet(wallet_name: String) -> Result<()> {
    let conn = get_db_connection()?;

    minotari_wallet::db::delete_account(&conn, &wallet_name).context("Failed to delete wallet")?;

    Ok(())
}

#[frb]
pub fn list_wallets() -> Result<Vec<String>> {
    let mut conn = get_db_connection()?;
    let accounts = get_accounts(&mut conn, None)
        .context("Failed to retrieve wallet accounts from database")?;

    let names = accounts.into_iter().map(|a| a.friendly_name).collect();

    Ok(names)
}

#[frb]
pub fn rename_wallet(current_wallet_name: String, new_wallet_name: String) -> Result<()> {
    let conn = get_db_connection()?;

    minotari_wallet::db::update_account_name(&conn, &current_wallet_name, &new_wallet_name)
        .context("Failed to rename wallet")?;

    Ok(())
}
