use crate::api::config::{DEFAULT_CONFIRMATION_WINDOW, DEFAULT_NUM_OUTPUTS};
use crate::api::db::get_db_pool;
use crate::api::error::WalletError;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::transactions::fee_estimator::{FeeEstimator, FeePriority as LibFeePriority};
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
