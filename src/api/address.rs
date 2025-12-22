use crate::api::{db::DB_PATH, network::parse_network};
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::{get_accounts, init_db};

#[frb]
pub fn get_address(
    name: Option<String>,
    passphrase: Option<String>,
    network: Option<String>,
) -> Result<String> {
    let network = parse_network(network)?;
    let db = DB_PATH.get().context("Database path not initialized")?;
    let pool = init_db(db)?;
    let mut conn = pool.get()?;
    let account = &get_accounts(&mut conn, name.as_deref())?[0];
    let address = account.get_address(network, passphrase.as_deref().unwrap_or(""))?;

    Ok(address.to_base58())
}
