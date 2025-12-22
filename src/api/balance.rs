use crate::api::db::DB_PATH;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::{get_accounts, get_balance as get_wallet_balance, init_db};

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
pub async fn get_balance(name: Option<String>) -> Result<AccountBalanceDto> {
    let db = DB_PATH.get().context("Database path not initialized")?;
    let pool = init_db(db)?;
    let mut conn = pool.get()?;
    let account = &get_accounts(&mut conn, name.as_deref())?[0];
    let agg_result = get_wallet_balance(&mut conn, account.id)?;
    Ok(agg_result.into())
}
