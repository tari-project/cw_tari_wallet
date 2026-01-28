use crate::api::db::{get_db_connection, get_db_path};
use crate::api::network::{parse_network, TariNetwork};
use anyhow::{anyhow, Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::get_accounts;
use minotari_wallet::utils::init_wallet::init_with_seed_words;
use std::str::FromStr;
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::{Mnemonic, MnemonicLanguage};
use tari_common_types::seeds::seed_words::SeedWords;
use tari_common_types::tari_address::{TariAddress, TariAddressFeatures};
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
    pub seed_words: SensitiveSeeds,
}

#[frb]
pub fn create_wallet(
    network: Option<TariNetwork>,
    passphrase: String,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);

    let network = parse_network(network);
    let seed = CipherSeed::random();
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, None).context("Failed to init wallet")?;

    Ok(details)
}

#[frb]
pub fn restore_wallet(
    seed_words: Vec<String>,
    passphrase: String,
    network: Option<TariNetwork>,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);
    let seed_words = Zeroizing::new(seed_words);

    let network = parse_network(network);

    let mnemonic_string = Zeroizing::new(seed_words.join(" "));
    let mnemonic = SeedWords::from_str(&mnemonic_string).context("Invalid seed words")?;
    let seed = CipherSeed::from_mnemonic(&mnemonic, None).context("Invalid cipher seed")?;
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, None).context("Failed to init wallet")?;

    Ok(details)
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
    let public_view_key = CompressedKey::from_secret_key(&view_key);

    let tari_address = TariAddress::new_dual_address(
        public_view_key,
        spend_key.pub_key.clone(),
        network,
        TariAddressFeatures::create_one_sided_only(),
        None,
    )
    .context("Failed to generate Tari address")?;

    let mnemonic = seed.to_mnemonic(MnemonicLanguage::English, None)?;
    let raw_words = split_hidden_words(mnemonic.join(" "));

    let sensitive_seeds = SensitiveSeeds { words: raw_words };

    Ok(WalletCreationDetails {
        tari_address: tari_address.to_base58(),
        wallet_birthday,
        spend_public_key_hex: spend_key.pub_key.to_hex(),
        view_private_key_hex: view_key.to_hex(),
        seed_words: sensitive_seeds,
    })
}

fn split_hidden_words(hidden: Hidden<String>) -> Vec<String> {
    hidden
        .reveal()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}
