use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::http::WalletHttpClient;
use tari_common_types::chain_metadata::ChainMetadata;
use tari_utilities::hex::Hex;

#[frb]
#[derive(Clone, Debug)]
pub struct TipInfo {
    /// The height of the best (most recent) block in the chain.
    ///
    /// This is the number of blocks from the genesis block to the tip.
    pub best_block_height: u64,

    /// The hash of the best block, as raw bytes.
    ///
    /// This uniquely identifies the current chain tip and can be used
    /// to detect chain reorganizations.
    pub best_block_hash: String,

    /// The pruning horizon in blocks.
    ///
    /// Blocks older than `best_block_height - pruning_horizon` may have
    /// been pruned and their full data may not be available.
    pub pruning_horizon: u64,

    /// The height up to which the chain has been pruned.
    ///
    /// Block data below this height may not be fully available.
    pub pruned_height: u64,

    /// The timestamp of the best block, in seconds since Unix epoch.
    pub timestamp: u64,
}

impl From<ChainMetadata> for TipInfo {
    fn from(m: ChainMetadata) -> Self {
        Self {
            best_block_height: m.best_block_height(),
            best_block_hash: m.best_block_hash().to_hex(),
            pruning_horizon: m.pruning_horizon(),
            pruned_height: m.pruned_height(),
            timestamp: m.timestamp(),
        }
    }
}

pub async fn get_tip_info(base_url: String) -> Result<Option<TipInfo>> {
    let base_url = base_url.parse().context("Failed to parse base URL")?;
    let wallet_client = WalletHttpClient::new(base_url)?;

    let tip_info = wallet_client.get_tip_info().await?;
    Ok(tip_info.metadata.map(Into::into))
}
