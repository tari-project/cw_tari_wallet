use crate::api::db::get_db_connection;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::{
    db::get_displayed_transactions_paginated, get_accounts, utils::timestamp::format_timestamp,
};

#[frb]
#[derive(Clone)]
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
#[derive(Clone)]
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
#[derive(Clone)]
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
#[derive(Clone)]
pub enum DisplayedTransactionDirection {
    Incoming,
    Outgoing,
}

impl From<minotari_wallet::transactions::TransactionDirection> for DisplayedTransactionDirection {
    fn from(d: minotari_wallet::transactions::TransactionDirection) -> Self {
        match d {
            minotari_wallet::transactions::TransactionDirection::Incoming => Self::Incoming,
            minotari_wallet::transactions::TransactionDirection::Outgoing => Self::Outgoing,
        }
    }
}

#[frb]
#[derive(Clone)]
pub enum DisplayedTransactionSource {
    Transfer,
    Coinbase,
    OneSided,
    Unknown,
}

impl From<minotari_wallet::transactions::TransactionSource> for DisplayedTransactionSource {
    fn from(s: minotari_wallet::transactions::TransactionSource) -> Self {
        match s {
            minotari_wallet::transactions::TransactionSource::Transfer => Self::Transfer,
            minotari_wallet::transactions::TransactionSource::Coinbase => Self::Coinbase,
            minotari_wallet::transactions::TransactionSource::OneSided => Self::OneSided,
            minotari_wallet::transactions::TransactionSource::Unknown => Self::Unknown,
        }
    }
}

#[frb]
#[derive(Clone)]
pub enum DisplayedTransactionStatus {
    Pending,
    Unconfirmed,
    Confirmed,
    Cancelled,
    Reorganized,
    Rejected,
}

impl From<minotari_wallet::transactions::TransactionDisplayStatus> for DisplayedTransactionStatus {
    fn from(s: minotari_wallet::transactions::TransactionDisplayStatus) -> Self {
        match s {
            minotari_wallet::transactions::TransactionDisplayStatus::Pending => Self::Pending,
            minotari_wallet::transactions::TransactionDisplayStatus::Unconfirmed => {
                Self::Unconfirmed
            }
            minotari_wallet::transactions::TransactionDisplayStatus::Confirmed => Self::Confirmed,
            minotari_wallet::transactions::TransactionDisplayStatus::Cancelled => Self::Cancelled,
            minotari_wallet::transactions::TransactionDisplayStatus::Reorganized => {
                Self::Reorganized
            }
            minotari_wallet::transactions::TransactionDisplayStatus::Rejected => Self::Rejected,
        }
    }
}

#[frb]
#[derive(Clone)]
pub struct DisplayedTransactionDto {
    pub id: String,
    pub direction: DisplayedTransactionDirection,
    pub source: DisplayedTransactionSource,
    pub status: DisplayedTransactionStatus,
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
            direction: t.direction.into(),
            source: t.source.into(),
            status: t.status.into(),
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
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, wallet_name.as_deref())?;
    let account = accounts
        .first()
        .context("No accounts found for this wallet")?;

    let transactions = get_displayed_transactions_paginated(&conn, account.id, limit, offset)?;

    Ok(transactions.into_iter().map(Into::into).collect())
}
