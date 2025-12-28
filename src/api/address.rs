use crate::api::{db::get_db_connection, network::parse_network};
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::get_accounts;

#[frb]
pub fn get_address(
    wallet_name: Option<String>,
    passphrase: Option<String>,
    network: Option<String>,
) -> Result<String> {
    let network = parse_network(network)?;
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, wallet_name.as_deref())?;
    let account = accounts
        .first()
        .context("No accounts found for this wallet")?;
    let address = account.get_address(network, passphrase.as_deref().unwrap_or(""))?;

    Ok(address.to_base58())
}
