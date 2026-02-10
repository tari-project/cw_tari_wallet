use crate::api::{db::get_db_connection, utils::format_micro_tari};
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::{
    db::get_displayed_transactions_paginated, get_accounts, utils::timestamp::format_timestamp,
};
#[frb]
#[derive(Debug, Clone)]
pub struct FeeInfoDto {
    pub amount: u64,
    pub amount_display: String,
}

impl From<minotari_wallet::transactions::FeeInfo> for FeeInfoDto {
    fn from(f: minotari_wallet::transactions::FeeInfo) -> Self {
        Self {
            amount: f.amount.0,
            amount_display: format_micro_tari(f.amount.0),
        }
    }
}

#[frb]
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
pub struct CounterpartyInfoDto {
    pub address: String,
    pub address_emoji: String,
}

impl From<tari_common_types::tari_address::TariAddress> for CounterpartyInfoDto {
    fn from(addr: tari_common_types::tari_address::TariAddress) -> Self {
        Self {
            address: addr.to_base58(),
            address_emoji: addr.to_emoji_string(),
        }
    }
}

#[frb]
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
    pub payrefs: Vec<String>,
}

impl From<minotari_wallet::DisplayedTransaction> for DisplayedTransactionDto {
    fn from(t: minotari_wallet::DisplayedTransaction) -> Self {
        Self {
            id: t.id.to_string(),
            direction: t.direction.into(),
            source: t.source.into(),
            status: t.status.into(),
            amount: t.amount.0,
            amount_display: format_micro_tari(t.amount.0),
            message: t.message,
            counterparty: t.counterparty.map(CounterpartyInfoDto::from),
            blockchain: t.blockchain.into(),
            fee: t.fee.map(FeeInfoDto::from),
            payrefs: t
                .details
                .sent_payrefs
                .iter()
                .map(|fixed_hash| fixed_hash.to_string())
                .collect(),
        }
    }
}

#[frb]
pub fn get_transactions(
    wallet_name: String,
    limit: i64,
    offset: i64,
) -> Result<Vec<DisplayedTransactionDto>> {
    let mut conn = get_db_connection()?;
    let accounts = &get_accounts(&mut conn, Some(&wallet_name))?;
    let account = accounts
        .first()
        .context("No accounts found for this wallet")?;

    let transactions = get_displayed_transactions_paginated(&conn, account.id, limit, offset)?;

    Ok(transactions.into_iter().map(Into::into).collect())
}
