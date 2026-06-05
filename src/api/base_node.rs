use std::time::Duration;

use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use minotari_wallet::http::WalletHttpClient;
use tari_common_types::chain_metadata::ChainMetadata;
use tari_transaction_components::rpc::models::TipInfoResponse;
use tari_utilities::hex::Hex;

use crate::api::config::{HEALTH_MAX_RETRIES, HEALTH_TIMEOUT_SECS};

/// A snapshot of the base node's chain tip, as reported over its HTTP RPC.
///
/// Returned by [`get_tip_info`]. All heights are block counts and `timestamp`
/// is seconds since the Unix epoch.
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

/// Connect to the base node at `base_url` and fetch its raw tip-info response over
/// HTTP RPC. Shared by [`get_tip_info`] and [`is_node_synced`] so the
/// parse → connect → fetch sequence (and its error strings) live in one place.
async fn fetch_tip_info(base_url: String) -> Result<TipInfoResponse> {
    let base_url = base_url.parse().context("Failed to parse base URL")?;
    let wallet_client = WalletHttpClient::new(base_url)?;
    wallet_client.get_tip_info().await
}

/// Fetch the base node's current chain tip over HTTP RPC.
///
/// `base_url` is the base node RPC endpoint (e.g. `https://rpc.tari.com`).
/// Returns `Ok(None)` when the node reports no chain metadata yet; `Ok(Some(_))`
/// otherwise.
///
/// Async; performs network I/O. Errors if `base_url` cannot be parsed or the RPC
/// request fails. Part of the frozen public contract (ledger D2) even though it
/// has no explicit `#[frb]`: FRB v2 exports every `pub` item in `crate::api`.
pub async fn get_tip_info(base_url: String) -> Result<Option<TipInfo>> {
    let tip_info = fetch_tip_info(base_url).await?;
    Ok(tip_info.metadata.map(Into::into))
}

/// Report whether the base node at `base_url` considers itself synced to the
/// network tip.
///
/// `base_url` is the base node RPC endpoint. Returns the node's own `is_synced`
/// flag from its tip-info response.
///
/// Async; performs network I/O. Errors if `base_url` cannot be parsed or the RPC
/// request fails. Part of the frozen public contract (ledger D2) even though it
/// has no explicit `#[frb]`.
pub async fn is_node_synced(base_url: String) -> Result<bool> {
    let tip_info = fetch_tip_info(base_url).await?;
    Ok(tip_info.is_synced)
}

/// Probe whether the base node at `base_url` is reachable (a `GET /get_tip_info`
/// round-trip succeeds). Liveness only, not sync status (use [`is_node_synced`]).
///
/// Returns `Ok(false)` — never `Err` — on parse failure, client construction
/// failure, unreachable node, or timeout, so Dart's `checkNodeHealth()` maps to
/// a plain `bool` with no `try`/`catch`. Uses a short timeout for snappy "test
/// connection" UX. Part of the frozen public contract (ledger D2) even though it
/// has no explicit `#[frb]`.
pub async fn check_node_health(base_url: String) -> Result<bool> {
    let url = match base_url.parse() {
        Ok(url) => url,
        // Bad URL is "not healthy", not an error: keep the Dart mapping a plain bool.
        Err(_) => return Ok(false),
    };
    let client = match WalletHttpClient::with_config(
        url,
        HEALTH_MAX_RETRIES,
        Duration::from_secs(HEALTH_TIMEOUT_SECS),
    ) {
        Ok(client) => client,
        // Client construction failure (e.g. TLS backend init) means no node can be
        // probed at all: report "not healthy" rather than throwing into Dart.
        Err(_) => return Ok(false),
    };
    // is_online maps Ok => true, Err (unreachable/timeout) => false.
    Ok(client.is_online().await)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    // Bad URL answers Ok(false), not Err; parse fails before any network call.
    #[tokio::test]
    async fn check_node_health_unparseable_url_is_false_not_err() {
        let result = check_node_health("not a url".to_string()).await;
        assert!(!result.unwrap());
    }
}
