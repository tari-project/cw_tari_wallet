use flutter_rust_bridge::frb;
use minotari_wallet::{get_accounts, init_db, get_balance as get_wallet_balance};
use crate::api::db::DB_PATH;

#[derive(Clone, Debug)]
pub struct AccountBalanceDto {
    pub unconfirmed: u64,
    pub locked: u64,
    pub available: u64,
}

impl From<minotari_wallet::db::AccountBalance> for AccountBalanceDto {
    fn from(b: minotari_wallet::db::AccountBalance) -> Self {
        Self {
            unconfirmed: b.unconfirmed,
            locked: b.locked,
            available: b.available,
        }
    }
}

#[frb]
pub async fn get_balance(name: String) -> anyhow::Result<AccountBalanceDto> {
    let db = DB_PATH.get().expect("should get db path");
    let pool = init_db(db).await?;
    let mut conn = pool.acquire().await?;
    let account = &get_accounts(&mut conn, Some(&name)).await?[0];
    let agg_result = get_wallet_balance(&mut conn, account.id).await?;
    Ok(agg_result.into())
}
