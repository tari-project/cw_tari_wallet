use crate::api::{
    db::get_db_connection,
    network::{parse_network, TariNetwork},
};
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::get_accounts;
use tari_common::network_check::set_network_if_choice_valid;

#[frb]
pub fn get_address(
    wallet_name: String,
    passphrase: Option<String>,
    network: Option<TariNetwork>,
) -> Result<String> {
    let network = parse_network(network);
    set_network_if_choice_valid(network)?;
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, Some(&wallet_name))?;
    let account = accounts
        .first()
        .context("No accounts found for this wallet")?;
    let address = account.get_address(network, passphrase.as_deref().unwrap_or(""))?;

    Ok(address.to_base58())
}
