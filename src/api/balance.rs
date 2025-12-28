use crate::api::db::get_db_connection;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::{get_accounts, get_balance as get_wallet_balance};

#[frb]
#[derive(Clone, Debug)]
pub struct AccountBalanceDto {
    pub total: u64,
    pub unconfirmed: u64,
    pub locked: u64,
    pub available: u64,
}

impl From<minotari_wallet::db::AccountBalance> for AccountBalanceDto {
    fn from(b: minotari_wallet::db::AccountBalance) -> Self {
        Self {
            total: b.total,
            unconfirmed: b.unconfirmed,
            locked: b.locked,
            available: b.available,
        }
    }
}

#[frb]
pub fn get_balance(wallet_name: Option<String>) -> Result<AccountBalanceDto> {
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, wallet_name.as_deref())?;
    let account = accounts
        .first()
        .context("No accounts found for this wallet")?;
    let agg_result = get_wallet_balance(&mut conn, account.id)?;
    Ok(agg_result.into())
}
