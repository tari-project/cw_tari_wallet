use crate::api::db::{get_db_connection, get_db_path};
use crate::api::error::WalletError;
use crate::api::network::{apply_network, parse_network, TariNetwork};
use crate::domain::address::{construct_wallet_address_details, split_hidden_words};
use crate::domain::keys::key_manager_from_cipher_seed;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::get_accounts;
use minotari_wallet::utils::init_wallet::{init_with_seed_words, init_with_view_key};
use std::str::FromStr;
use tari_common::configuration::Network;
use tari_common_types::seeds::cipher_seed::CipherSeed;
use tari_common_types::seeds::mnemonic::{Mnemonic, MnemonicLanguage};
use tari_common_types::seeds::seed_words::SeedWords;
use tari_common_types::types::{CompressedPublicKey, PrivateKey};
use tari_transaction_components::key_manager::TransactionKeyManagerInterface;
use tari_utilities::hex::Hex;
use tari_utilities::hidden::Hidden;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// **SECRET.** An opaque, zeroize-on-drop handle to a wallet's seed words.
///
/// Exposed to Dart as an opaque handle (`#[frb(opaque)]`): the words are never
/// copied across the bridge implicitly. Call [`reveal_seed_words`] to read them
/// out explicitly. The backing buffer is wiped on drop ([`ZeroizeOnDrop`]).
#[frb(opaque)]
#[derive(Clone, Debug, Zeroize, ZeroizeOnDrop)]
pub struct SensitiveSeeds {
    pub words: Vec<String>,
}

/// The public details returned after creating, restoring, or importing a wallet.
///
/// `tari_address` is the wallet's base58 dual address. `wallet_birthday` is the
/// cipher-seed birthday (a block-height-derived epoch the scanner starts from).
/// The key hexes are the public spend key and the **private** view key. For
/// view-only imports `seed_words` is `None`; otherwise it carries the
/// **secret** [`SensitiveSeeds`] handle.
#[frb]
pub struct WalletCreationDetails {
    pub tari_address: String,
    pub wallet_birthday: u16,
    pub spend_public_key_hex: String,
    pub view_private_key_hex: String,
    pub seed_words: Option<SensitiveSeeds>,
}

/// Create a brand-new wallet from a freshly generated random cipher seed.
///
/// `wallet_name` names the account in the DB; `network` selects the Tari network
/// (`None` resolves to MainNet — frozen behavior); `passphrase` (**secret**)
/// encrypts the wallet at rest. Requires [`initialize_database`] first.
/// Returns the new wallet's address, keys, and freshly generated seed words.
/// Synchronous.
#[frb]
pub fn create_wallet(
    wallet_name: String,
    network: Option<TariNetwork>,
    passphrase: String,
) -> Result<WalletCreationDetails> {
    let password = Zeroizing::new(passphrase);

    let network = parse_network(network);
    apply_network(network)?;
    let seed = CipherSeed::random();
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, Some(&wallet_name))
        .context("Failed to init wallet")?;

    Ok(details)
}

/// Restore an existing wallet from its BIP-39-style seed words.
///
/// `seed_words` (**secret**) is the mnemonic to restore from; `passphrase`
/// (**secret**) re-encrypts the restored wallet; `network` selects the network
/// (`None` → MainNet). Requires [`initialize_database`] first. Returns the
/// restored wallet's address, keys, and seed words. Synchronous; errors on
/// invalid seed words.
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
    apply_network(network)?;

    // Intentionally NOT routed through `domain::keys::key_manager_from_seed_words`:
    // restore needs the `CipherSeed` itself (for `init_with_seed_words` and the
    // birthday), not just a `KeyManager`, and — load-bearing — these `.context(...)`
    // strings are the FROZEN Dart-visible errors ("Invalid seed words" / "Invalid
    // cipher seed"), which differ from the domain helper's `WalletError` text. Sharing
    // the code would change those strings and break the contract; keep them separate.
    let mnemonic_string = Zeroizing::new(seed_words.join(" "));
    let mnemonic = SeedWords::from_str(&mnemonic_string).context("Invalid seed words")?;
    let seed = CipherSeed::from_mnemonic(&mnemonic, None).context("Invalid cipher seed")?;
    let db_path = get_db_path()?;

    let details = generate_details_from_seed(&seed, network)?;

    init_with_seed_words(seed, &password, &db_path, Some(&wallet_name))
        .context("Failed to init wallet")?;

    Ok(details)
}

/// Import a view-only wallet from a view private key and spend public key.
///
/// A view-only wallet can see incoming funds but cannot spend. `view_private_key_hex`
/// (**secret**) and `spend_public_key_hex` are hex-encoded keys; `birthday` is the
/// scan start epoch; `passphrase` (**secret**) encrypts the wallet; `network`
/// selects the network (`None` → MainNet). Requires [`initialize_database`] first.
/// The returned `seed_words` is always `None`. Synchronous; errors on invalid hex.
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
    let network = parse_network(network);
    apply_network(network)?;

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
        .map_err(|_| WalletError::internal("Invalid hex for view key"))?;
    let spend_key = CompressedPublicKey::from_hex(&spend_public_key_hex)
        .map_err(|_| WalletError::internal("Invalid hex for spend key"))?;

    build_creation_details(spend_key, view_key, birthday, network, None)
}

/// Recover the seed words of the currently stored wallet.
///
/// `password` (**secret**) is the wallet's encryption passphrase, required to
/// decrypt the stored cipher seed. Returns a **secret**, zeroize-on-drop
/// [`SensitiveSeeds`] handle; call [`reveal_seed_words`] to read the words.
/// Requires [`initialize_database`] first. Synchronous; errors if there is no
/// account or no seed words (e.g. a view-only wallet).
#[frb]
pub fn get_seed_words(password: String) -> Result<SensitiveSeeds> {
    let password = Zeroizing::new(password);

    let conn = get_db_connection()?;
    let accounts = get_accounts(&conn, None)?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;

    let seed_words_obj = account
        .get_seed_words(&password)?
        .ok_or_else(|| WalletError::internal("Account does not have seed words"))?;

    let hidden_joined = Hidden::from(seed_words_obj.join(" "));
    let raw_words = split_hidden_words(hidden_joined);

    Ok(SensitiveSeeds { words: raw_words })
}

/// Explicitly read the seed words out of an opaque [`SensitiveSeeds`] handle.
///
/// This is the only way to move the **secret** words across the bridge as plain
/// strings — callers opt in deliberately. Synchronous; infallible.
#[frb]
pub fn reveal_seed_words(handle: &SensitiveSeeds) -> Vec<String> {
    handle.words.clone()
}

fn generate_details_from_seed(
    seed: &CipherSeed,
    network: Network,
) -> Result<WalletCreationDetails> {
    let wallet_birthday = seed.birthday();

    // Single key-manager factory (domain layer) — no duplicated derivation.
    let key_manager = key_manager_from_cipher_seed(seed.clone())?;

    let view_key = key_manager.get_private_view_key();
    let spend_key = key_manager.get_spend_key();

    let mnemonic = seed.to_mnemonic(MnemonicLanguage::English, None)?;
    let raw_words = split_hidden_words(mnemonic.join(" "));
    let sensitive_seeds = SensitiveSeeds { words: raw_words };

    build_creation_details(
        spend_key.pub_key,
        view_key,
        wallet_birthday,
        network,
        Some(sensitive_seeds),
    )
}

/// Assemble the public `WalletCreationDetails` DTO from the domain address
/// computation. The DTO (the wire contract) stays in `api`; the domain only
/// returns the computed address/key values.
fn build_creation_details(
    public_spend_key: CompressedPublicKey,
    private_view_key: PrivateKey,
    birthday: u16,
    network: Network,
    seed_words: Option<SensitiveSeeds>,
) -> Result<WalletCreationDetails> {
    let address = construct_wallet_address_details(public_spend_key, private_view_key, network)?;

    Ok(WalletCreationDetails {
        tari_address: address.tari_address,
        wallet_birthday: birthday,
        spend_public_key_hex: address.spend_public_key_hex,
        view_private_key_hex: address.view_private_key_hex,
        seed_words,
    })
}

/// Delete the wallet account named `wallet_name` from the database.
///
/// Requires [`initialize_database`] first. Synchronous; errors if the delete
/// fails.
#[frb]
pub fn delete_wallet(wallet_name: String) -> Result<()> {
    let conn = get_db_connection()?;

    minotari_wallet::db::delete_account(&conn, &wallet_name).context("Failed to delete wallet")?;

    Ok(())
}

/// List the friendly names of every wallet account in the database.
///
/// Requires [`initialize_database`] first. Synchronous.
#[frb]
pub fn list_wallets() -> Result<Vec<String>> {
    let mut conn = get_db_connection()?;
    let accounts = get_accounts(&mut conn, None)
        .context("Failed to retrieve wallet accounts from database")?;

    let names = accounts.into_iter().map(|a| a.friendly_name).collect();

    Ok(names)
}

/// Rename a wallet account from `current_wallet_name` to `new_wallet_name`.
///
/// Requires [`initialize_database`] first. Synchronous; errors if the update
/// fails.
#[frb]
pub fn rename_wallet(current_wallet_name: String, new_wallet_name: String) -> Result<()> {
    let conn = get_db_connection()?;

    minotari_wallet::db::update_account_name(&conn, &current_wallet_name, &new_wallet_name)
        .context("Failed to rename wallet")?;

    Ok(())
}
