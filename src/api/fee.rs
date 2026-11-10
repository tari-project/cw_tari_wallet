use crate::api::config::{
    DEFAULT_BASE_URL, DEFAULT_CONFIRMATION_WINDOW, DEFAULT_NUM_OUTPUTS, MAX_FEE_PER_GRAM,
    SEND_FEE_PER_GRAM,
};
use crate::api::db::get_db_pool;
use crate::api::error::WalletError;
use crate::api::network::{parse_network, TariNetwork};
use crate::domain::validation::{validate_fee_per_gram, validate_recipient};
use anyhow::{anyhow, Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::db::{get_account_by_name, SqlitePool};
use minotari_wallet::http::WalletHttpClient;
use minotari_wallet::transactions::fee_estimator::{
    estimated_output_size_for_send, FeeEstimator, FeePriority as LibFeePriority,
};
use minotari_wallet::transactions::fund_locker::MIN_FEE_PER_GRAM;
use minotari_wallet::transactions::input_selector::InputSelector;
use tari_common_types::tari_address::TariAddress;
use tari_transaction_components::fee::Fee;
use tari_transaction_components::weight::TransactionWeight;
use tari_transaction_components::MicroMinotari;

/// Desired confirmation speed for a transaction, trading fee against latency:
/// `Slow` (cheapest), `Medium`, or `Fast` (most expensive).
#[frb]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeePriority {
    Slow,
    Medium,
    Fast,
}

impl From<FeePriority> for LibFeePriority {
    fn from(p: FeePriority) -> Self {
        match p {
            FeePriority::Slow => LibFeePriority::Slow,
            FeePriority::Medium => LibFeePriority::Medium,
            FeePriority::Fast => LibFeePriority::Fast,
        }
    }
}

/// A fee estimate for a prospective send.
///
/// `estimated_fee` and `total_amount_required` (amount + fee) are in **microTari**;
/// `fee_per_gram` is the rate in microTari per gram of transaction weight (the node's
/// rate, capped at the most a send accepts, so it can be passed to
/// [`send_transaction_at_fee`](crate::api::send_transaction::send_transaction_at_fee),
/// which builds at exactly that rate; the fee it pays can still differ from
/// `estimated_fee`, which sizes a plain output without a payment id);
/// `input_count` is how many UTXOs would be spent.
#[frb]
#[derive(Debug, Clone)]
pub struct FeeEstimate {
    pub estimated_fee: u64,
    pub total_amount_required: u64,
    pub fee_per_gram: u64,
    pub input_count: usize,
}

/// Estimate the fee to send `amount` (in **microTari**) at the given `priority`.
///
/// `base_url` is the base node RPC endpoint and `wallet_name` selects the funding
/// account. Requires
/// a database path (`set_database_path` or `unlock_wallet`) first. Async;
/// performs network I/O. Errors if estimation fails or no estimate matches the
/// requested priority.
#[frb]
pub async fn estimate_transaction_fee(
    amount: u64,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
) -> Result<FeeEstimate> {
    estimate_fee(amount, DEFAULT_NUM_OUTPUTS, priority, base_url, wallet_name).await
}

/// Estimate the fee to pay `num_outputs` recipients a total of `amount` (in
/// **microTari**) in one transaction, at the given `priority`.
///
/// As [`estimate_transaction_fee`], which is this with `num_outputs = 1`. Use it to
/// quote a [`send_transaction`](crate::api::send_transaction::send_transaction).
#[frb]
pub async fn estimate_transaction_fee_for_outputs(
    amount: u64,
    num_outputs: u32,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
) -> Result<FeeEstimate> {
    let num_outputs = usize::try_from(num_outputs)
        .map_err(|_| WalletError::wallet("Too many outputs to estimate a fee for"))?;
    estimate_fee(amount, num_outputs, priority, base_url, wallet_name).await
}

async fn estimate_fee(
    amount: u64,
    num_outputs: usize,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
) -> Result<FeeEstimate> {
    let pool = get_db_pool()?;
    capped_estimate(
        pool,
        &base_url,
        &wallet_name,
        MicroMinotari(amount),
        num_outputs,
        priority,
    )
    .await
    .context("Failed to estimate fees")
}

/// minotari's `FeeEstimator::estimate_fees` for one `priority`, except that the node's
/// rates are capped at [`MAX_FEE_PER_GRAM`] before inputs are selected, so a wallet that
/// can pay at the cap gets a quote. Kept step for step with upstream (pinned against it in
/// the verify harness): at or below the cap the estimate is upstream's.
async fn capped_estimate(
    pool: SqlitePool,
    base_url: &str,
    wallet_name: &str,
    amount: MicroMinotari,
    num_outputs: usize,
    priority: FeePriority,
) -> Result<FeeEstimate> {
    let conn = pool.get()?;
    let account = get_account_by_name(&conn, wallet_name)?
        .ok_or_else(|| anyhow!("Account with name '{}' not found", wallet_name))?;

    let client = WalletHttpClient::new(base_url.parse()?)?;
    let stats = client.get_mempool_fee_per_gram_stats(3).await.ok();
    // Fast: the next block's average; Medium: the second block's; Slow: the deepest
    // block's minimum. No stats: minotari's defaults.
    let (fast, medium, slow) = match stats.as_deref() {
        Some([first, rest @ ..]) => (
            first.avg_fee_per_gram,
            rest.first().unwrap_or(first).avg_fee_per_gram,
            rest.last().unwrap_or(first).min_fee_per_gram,
        ),
        _ => (MicroMinotari(10), MicroMinotari(5), MicroMinotari(1)),
    };
    let (fast, medium, slow) = (cap(fast), cap(medium), cap(slow));

    let selection = InputSelector::new(account.id, DEFAULT_CONFIRMATION_WINDOW)
        .fetch_unspent_outputs(&conn, amount, num_outputs, fast, None)?;
    let input_count = selection.utxos.len();
    let total_outputs = if selection.requires_change_output {
        num_outputs
            .checked_add(1)
            .ok_or_else(|| anyhow!("Too many outputs to estimate a fee for"))?
    } else {
        num_outputs
    };
    let output_size = estimated_output_size_for_send(0, num_outputs, &TariAddress::default())?;
    let total_output_size = output_size
        .checked_mul(total_outputs)
        .ok_or_else(|| anyhow!("Total output size overflows while estimating fee"))?;

    let fee_per_gram = match priority {
        FeePriority::Slow => slow,
        FeePriority::Medium => medium,
        FeePriority::Fast => fast,
    };
    let fee = Fee::new(TransactionWeight::latest()).calculate(
        fee_per_gram,
        1,
        input_count,
        total_outputs,
        total_output_size,
    );
    let total_amount_required = amount
        .checked_add(fee)
        .ok_or_else(|| anyhow!("Amount plus fee overflows while estimating fee"))?;

    Ok(FeeEstimate {
        estimated_fee: fee.0,
        total_amount_required: total_amount_required.0,
        fee_per_gram: fee_per_gram.0,
        input_count,
    })
}

/// A node rate as a send rate: at least minotari's minimum (the node's integer averages can
/// be 0) and at most [`MAX_FEE_PER_GRAM`].
fn cap(rate: MicroMinotari) -> MicroMinotari {
    rate.max(MIN_FEE_PER_GRAM)
        .min(MicroMinotari(MAX_FEE_PER_GRAM))
}

/// What a send-all would cost and pay, from [`estimate_send_all_fee`].
///
/// `fee` and `amount_after_fee` (what the recipient receives) are in **microTari**;
/// `input_count` is how many UTXOs it spends. `skipped_dust` outputs are left behind
/// for being worth less than they cost to spend, and `skipped_over_cap` for not
/// fitting one transaction (another send-all can sweep those).
#[frb]
#[derive(Debug, Clone)]
pub struct SendAllFeeEstimate {
    pub fee: u64,
    pub amount_after_fee: u64,
    pub input_count: usize,
    pub skipped_dust: usize,
    pub skipped_over_cap: usize,
}

/// Quote a send-all of wallet `wallet_name` to `recipient_address`.
///
/// Computed by minotari exactly as the send-all in
/// [`send_transaction`](crate::api::send_transaction::send_transaction)
/// computes it, at the same fee per gram, so `amount_after_fee` is what that send
/// pays. To send at another rate, quote with [`estimate_send_all_fee_at`]. `payment_id` changes the output size and so the fee; pass the one the send
/// will use, and the same `confirmation_window` (`None` → 3, as for the send).
/// Offline: reads only the wallet database. `network` as elsewhere (`None` → MainNet). Errors if the address is invalid or the wallet's spendable
/// balance would leave the recipient only dust. Requires
/// a database path (`set_database_path` or `unlock_wallet`) first.
#[frb]
pub fn estimate_send_all_fee(
    wallet_name: String,
    recipient_address: String,
    payment_id: Option<String>,
    network: Option<TariNetwork>,
    confirmation_window: Option<u64>,
) -> Result<SendAllFeeEstimate> {
    estimate_send_all_fee_at(
        wallet_name,
        recipient_address,
        payment_id,
        network,
        confirmation_window,
        SEND_FEE_PER_GRAM,
    )
}

/// [`estimate_send_all_fee`] at `fee_per_gram` (µT per gram), quoting the send-all of
/// [`send_transaction_at_fee`](crate::api::send_transaction::send_transaction_at_fee)
/// at the same rate. Errors with `Invalid fee per gram: …` for a rate of 0 or above the
/// cap, before the wallet is read.
#[frb]
pub fn estimate_send_all_fee_at(
    wallet_name: String,
    recipient_address: String,
    payment_id: Option<String>,
    network: Option<TariNetwork>,
    confirmation_window: Option<u64>,
    fee_per_gram: u64,
) -> Result<SendAllFeeEstimate> {
    validate_fee_per_gram(fee_per_gram, MAX_FEE_PER_GRAM)?;
    let network = parse_network(network);
    let address = validate_recipient(network, &recipient_address, 0, true)?;

    // The estimator never contacts the node for a send-all quote.
    let estimator = FeeEstimator::new(get_db_pool()?, DEFAULT_BASE_URL.to_string());
    let estimate = estimator
        .estimate_fees_send_all(
            &wallet_name,
            network,
            &address,
            payment_id.as_deref(),
            MicroMinotari(fee_per_gram),
            confirmation_window.unwrap_or(DEFAULT_CONFIRMATION_WINDOW),
        )
        .map_err(|e| WalletError::wallet(e.to_string()))?;

    Ok(SendAllFeeEstimate {
        fee: estimate.fee.0,
        amount_after_fee: estimate.amount_after_fee.0,
        input_count: estimate.input_count,
        skipped_dust: estimate.skipped_dust,
        skipped_over_cap: estimate.skipped_over_cap,
    })
}

#[cfg(test)]
mod tests {
    //! Enum-mapping tests. Pure conversion, no I/O. Upstream-drift tripwire:
    //! if `minotari`'s `FeePriority` adds/renames a variant, this must change.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn fee_priority_maps_to_lib_priority_exhaustively() {
        assert_eq!(
            LibFeePriority::from(FeePriority::Slow),
            LibFeePriority::Slow
        );
        assert_eq!(
            LibFeePriority::from(FeePriority::Medium),
            LibFeePriority::Medium
        );
        assert_eq!(
            LibFeePriority::from(FeePriority::Fast),
            LibFeePriority::Fast
        );
    }

    #[test]
    fn node_rates_are_held_between_the_minimum_and_the_cap() {
        assert_eq!(cap(MicroMinotari(0)), MIN_FEE_PER_GRAM);
        assert_eq!(cap(MicroMinotari(5)), MicroMinotari(5));
        assert_eq!(
            cap(MicroMinotari(MAX_FEE_PER_GRAM)),
            MicroMinotari(MAX_FEE_PER_GRAM)
        );
        assert_eq!(
            cap(MicroMinotari(MAX_FEE_PER_GRAM + 1)),
            MicroMinotari(MAX_FEE_PER_GRAM)
        );
        assert_eq!(
            cap(MicroMinotari(u64::MAX)),
            MicroMinotari(MAX_FEE_PER_GRAM)
        );
    }
}
