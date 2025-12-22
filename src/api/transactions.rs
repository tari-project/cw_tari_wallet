use crate::api::db::DB_PATH;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::{
    db::get_displayed_transactions_paginated, get_accounts, init_db, utils::format_timestamp,
};

#[frb]
pub struct FeeInfoDto {
    pub amount: u64,
    pub amount_display: String,
}

impl From<minotari_wallet::transactions::FeeInfo> for FeeInfoDto {
    fn from(f: minotari_wallet::transactions::FeeInfo) -> Self {
        Self {
            amount: f.amount,
            amount_display: f.amount_display,
        }
    }
}

#[frb]
pub struct BlockchainInfoDto {
    pub block_height: u64,
    pub timestamp: String,
    pub confirmations: u64,
}

impl From<minotari_wallet::transactions::BlockchainInfo> for BlockchainInfoDto {
    fn from(i: minotari_wallet::transactions::BlockchainInfo) -> Self {
        Self {
            block_height: i.block_height,
            timestamp: format_timestamp(i.timestamp),
            confirmations: i.confirmations,
        }
    }
}

#[frb]
pub struct CounterpartyInfoDto {
    pub address: String,
    pub address_emoji: Option<String>,
    pub label: Option<String>,
}

impl From<minotari_wallet::transactions::CounterpartyInfo> for CounterpartyInfoDto {
    fn from(i: minotari_wallet::transactions::CounterpartyInfo) -> Self {
        Self {
            address: i.address,
            address_emoji: i.address_emoji,
            label: i.label,
        }
    }
}

#[frb]
pub struct DisplayedTransactionDto {
    pub id: String,
    pub direction: String,
    pub source: String,
    pub status: String,
    pub amount: u64,
    pub amount_display: String,
    pub message: Option<String>,
    pub counterparty: Option<CounterpartyInfoDto>,
    pub blockchain: BlockchainInfoDto,
    pub fee: Option<FeeInfoDto>,
}

impl From<minotari_wallet::DisplayedTransaction> for DisplayedTransactionDto {
    fn from(t: minotari_wallet::DisplayedTransaction) -> Self {
        Self {
            id: t.id,
            direction: t.direction.as_label().to_string(),
            source: t.source.as_label().to_string(),
            status: t.status.as_label().to_string(),
            amount: t.amount,
            amount_display: t.amount_display,
            message: t.message,
            counterparty: t.counterparty.map(CounterpartyInfoDto::from),
            blockchain: t.blockchain.into(),
            fee: t.fee.map(FeeInfoDto::from),
        }
    }
}

#[frb]
pub fn get_transactions(
    wallet_name: Option<String>,
    limit: i64,
    offset: i64,
) -> Result<Vec<DisplayedTransactionDto>> {
    let db = DB_PATH.get().context("Database path not initialized")?;
    let pool = init_db(db)?;
    let mut conn = pool.get()?;
    let account = &get_accounts(&mut conn, wallet_name.as_deref())?[0];

    let transactions = get_displayed_transactions_paginated(&conn, account.id, limit, offset)?;

    Ok(transactions
        .into_iter()
        .map(DisplayedTransactionDto::from)
        .collect())
}
