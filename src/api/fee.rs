use crate::api::config::{DEFAULT_CONFIRMATION_WINDOW, DEFAULT_NUM_OUTPUTS};
use crate::api::db::get_db_pool;
use crate::api::error::WalletError;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::transactions::fee_estimator::{FeeEstimator, FeePriority as LibFeePriority};
use tari_transaction_components::MicroMinotari;

/// Estimate a concrete one-recipient payment using its selected rate and memo.
/// Unlike the legacy estimator, UTXO selection uses the requested priority rather
/// than Fast for every priority. This does not reserve funds or broadcast.
#[frb]
pub async fn estimate_payment_fee(
    amount: u64,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
    payment_id: Option<String>,
) -> Result<FeeEstimate> {
    use minotari_wallet::{
        db::get_account_by_name,
        http::WalletHttpClient,
        transactions::{
            fee_estimator::estimated_output_size_for_payment_id, input_selector::InputSelector,
        },
    };

    if amount == 0 {
        anyhow::bail!("Payment amount must be greater than zero");
    }
    let client =
        WalletHttpClient::with_config(base_url.parse()?, 0, std::time::Duration::from_secs(5))?;
    // Same rate policy/fallback as upstream FeeEstimator. Selection below uses
    // the chosen rate, so a valid Slow send is not rejected for lacking Fast fees.
    let rate = match client.get_mempool_fee_per_gram_stats(3).await {
        Ok(stats) if !stats.is_empty() => match priority {
            FeePriority::Fast => stats[0].avg_fee_per_gram,
            FeePriority::Medium => stats.get(1).unwrap_or(&stats[0]).avg_fee_per_gram,
            FeePriority::Slow => stats[stats.len() - 1].min_fee_per_gram,
        },
        _ => MicroMinotari(match priority {
            FeePriority::Fast => 10,
            FeePriority::Medium => 5,
            FeePriority::Slow => 1,
        }),
    }
    .max(minotari_wallet::transactions::fund_locker::MIN_FEE_PER_GRAM);
    let output_size =
        estimated_output_size_for_payment_id(payment_id.as_ref().map_or(0, String::len))?;
    let pool = get_db_pool()?;
    let conn = pool.get()?;
    let account = get_account_by_name(&conn, &wallet_name)?.ok_or(WalletError::NoAccounts)?;
    let selection = InputSelector::new(account.id, DEFAULT_CONFIRMATION_WINDOW)
        .fetch_unspent_outputs(&conn, MicroMinotari(amount), 1, rate, Some(output_size))?;
    let fee = selection.fee();
    let total = amount
        .checked_add(fee.0)
        .ok_or_else(|| anyhow::anyhow!("Amount plus fee overflows while estimating payment"))?;
    Ok(FeeEstimate {
        estimated_fee: fee.0,
        total_amount_required: total,
        fee_per_gram: rate.0,
        input_count: selection.utxos.len(),
    })
}

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
/// `fee_per_gram` is the rate in microTari per gram of transaction weight;
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
/// [`initialize_database`](crate::api::db::initialize_database) first. Async;
/// performs network I/O. Errors if estimation fails or no estimate matches the
/// requested priority.
#[frb]
pub async fn estimate_transaction_fee(
    amount: u64,
    priority: FeePriority,
    base_url: String,
    wallet_name: String,
) -> Result<FeeEstimate> {
    let pool = get_db_pool()?;

    let estimator = FeeEstimator::new(pool, base_url);

    let amount_micro = MicroMinotari(amount);

    let estimates = estimator
        .estimate_fees(
            &wallet_name,
            amount_micro,
            DEFAULT_NUM_OUTPUTS,
            DEFAULT_CONFIRMATION_WINDOW,
            None, // estimated_output_size: let the estimator use its own default
        )
        .await
        .context("Failed to estimate fees")?;

    let target_priority: LibFeePriority = priority.into();

    let selected_estimate = estimates
        .into_iter()
        .find(|e| e.priority == target_priority)
        .ok_or_else(|| {
            WalletError::internal("Could not find fee estimate for requested priority")
        })?;

    Ok(FeeEstimate {
        estimated_fee: selected_estimate.estimated_fee.0,
        total_amount_required: selected_estimate.total_amount_required.0,
        fee_per_gram: selected_estimate.fee_per_gram.0,
        input_count: selected_estimate.input_count,
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
}
