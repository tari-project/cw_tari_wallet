use crate::api::db::get_db_pool;
use anyhow::{anyhow, Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::transactions::fee_estimator::{FeeEstimator, FeePriority as LibFeePriority};
use tari_transaction_components::MicroMinotari;

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

#[frb]
#[derive(Debug, Clone)]
pub struct FeeEstimate {
    pub estimated_fee: u64,
    pub total_amount_required: u64,
    pub fee_per_gram: u64,
    pub input_count: usize,
}

#[frb]
pub async fn estimate_transaction_fee(
    amount: u64,
    priority: FeePriority,
    base_url: String,
    wallet_name: Option<String>,
) -> Result<FeeEstimate> {
    let pool = get_db_pool()?;

    let estimator = FeeEstimator::new(pool, base_url);

    let amount_micro = MicroMinotari(amount);
    let account_name = wallet_name.as_deref().unwrap_or("default");

    let num_outputs = 1;
    let confirmation_window = 3;
    let estimated_output_size = None;

    let estimates = estimator
        .estimate_fees(
            account_name,
            amount_micro,
            num_outputs,
            confirmation_window,
            estimated_output_size,
        )
        .await
        .context("Failed to estimate fees")?;

    let target_priority: LibFeePriority = priority.into();

    let selected_estimate = estimates
        .into_iter()
        .find(|e| e.priority == target_priority)
        .ok_or_else(|| anyhow!("Could not find fee estimate for requested priority"))?;

    Ok(FeeEstimate {
        estimated_fee: selected_estimate.estimated_fee.0,
        total_amount_required: selected_estimate.total_amount_required.0,
        fee_per_gram: selected_estimate.fee_per_gram.0,
        input_count: selected_estimate.input_count,
    })
}
