use crate::api::error::WalletError;
use crate::api::{
    db::get_db_connection,
    network::{apply_network, parse_network, TariNetwork},
};
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::get_accounts;

/// Return the base58 Tari address of the wallet account named `wallet_name`.
///
/// `passphrase` (**secret**, optional) decrypts the account if needed; `network`
/// selects the network the address is encoded for (`None` → MainNet). Requires
/// [`initialize_database`](crate::api::db::initialize_database) first. Synchronous;
/// errors if the account does not exist.
#[frb]
pub fn get_address(
    wallet_name: String,
    passphrase: Option<String>,
    network: Option<TariNetwork>,
) -> Result<String> {
    let network = parse_network(network);
    apply_network(network)?;
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, Some(&wallet_name))?;
    let account = accounts.first().ok_or(WalletError::NoAccounts)?;
    let address = account.get_address(network, passphrase.as_deref().unwrap_or(""))?;

    Ok(address.to_base58())
}
