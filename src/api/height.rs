//! Conversions between dates, block heights and the wallet birthday.
//!
//! Thin adapter over `minotari::utils::height`, so Dart never computes heights or
//! days-since-genesis itself.

use crate::api::error::WalletError;
use crate::api::network::{parse_network, TariNetwork};
use anyhow::Result;
use flutter_rust_bridge::frb;
use minotari_wallet::utils::height;

/// Ask the base node at `base_url` for the height of the block mined at `epoch_secs`
/// (seconds since the Unix epoch) on `network` (`None` → MainNet).
///
/// The answer is checked against [`estimate_height_at_time`], which runs high: a
/// height more than 10% plus a day of blocks above it is refused, so a hostile node
/// cannot push a restore or rescan start past the wallet's funds. Async; performs
/// network I/O. Errors as `Network Error: …` if the URL is invalid, the node cannot
/// answer, or its answer is implausible.
#[frb]
pub async fn get_height_at_time(
    base_url: String,
    epoch_secs: u64,
    network: Option<TariNetwork>,
) -> Result<u64> {
    let node_height = height::height_at_time(&base_url, epoch_secs)
        .await
        .map_err(|e| WalletError::network(e.to_string()))?;
    let estimate = height::estimate_height_at_time(parse_network(network), epoch_secs);
    let max_plausible = max_plausible_height(estimate);
    if node_height > max_plausible {
        return Err(WalletError::network(format!(
            "Node reported an implausible height {node_height} for that time; expected at most {max_plausible}"
        ))
        .into());
    }
    Ok(node_height)
}

/// The highest node answer accepted for an offline `estimate`: 10% above it, plus a
/// day of blocks for times close to genesis.
fn max_plausible_height(estimate: u64) -> u64 {
    const DAY_OF_BLOCKS: u64 = 24 * 60 * 60 / height::TARGET_BLOCK_TIME_SECS;
    estimate
        .saturating_add(estimate / 10)
        .saturating_add(DAY_OF_BLOCKS)
}

/// Estimate, without a node, the height of the block mined at `epoch_secs` on
/// `network` (`None` → MainNet).
///
/// Assumes the target block time, so it tends to run a few percent above the real
/// height; subtract a margin before using it as a restore or rescan start.
#[frb(sync)]
pub fn estimate_height_at_time(network: Option<TariNetwork>, epoch_secs: u64) -> u64 {
    height::estimate_height_at_time(parse_network(network), epoch_secs)
}

/// The wallet birthday (days since the birthday genesis) for a wallet whose first
/// block is `height` on `network` (`None` → MainNet). Pass it as `birthday` to
/// [`import_wallet_from_keys`](crate::api::wallet::import_wallet_from_keys) or
/// [`import_view_only_wallet`](crate::api::wallet::import_view_only_wallet).
#[frb(sync)]
pub fn birthday_from_height(network: Option<TariNetwork>, height: u64) -> u16 {
    height::birthday_from_height(parse_network(network), height)
}

/// The wallet birthday (days since the birthday genesis) for a wallet created at
/// `epoch_secs` (seconds since the Unix epoch).
#[frb(sync)]
pub fn birthday_from_date(epoch_secs: u64) -> u16 {
    height::birthday_from_time(epoch_secs)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const MAINNET_GENESIS: u64 = 1_746_511_200;
    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn estimate_height_counts_two_minute_blocks_since_genesis() {
        assert_eq!(estimate_height_at_time(None, 0), 0);
        assert_eq!(
            estimate_height_at_time(Some(TariNetwork::MainNet), MAINNET_GENESIS + DAY),
            720
        );
    }

    #[test]
    fn birthday_from_date_and_height_agree() {
        let at_genesis = birthday_from_date(MAINNET_GENESIS);
        assert_eq!(
            birthday_from_height(Some(TariNetwork::MainNet), 0),
            at_genesis
        );
        assert_eq!(
            birthday_from_height(Some(TariNetwork::MainNet), 720 * 10),
            at_genesis + 10
        );
        assert_eq!(birthday_from_date(u64::MAX), u16::MAX);
    }

    #[test]
    fn plausible_height_allows_ten_percent_and_a_day() {
        assert_eq!(max_plausible_height(0), 720);
        assert_eq!(max_plausible_height(100_000), 110_720);
        assert_eq!(max_plausible_height(u64::MAX), u64::MAX);
    }

    #[tokio::test]
    async fn get_height_at_time_bad_url_is_network_error() {
        let err = get_height_at_time("not a url".to_string(), 0, None)
            .await
            .expect_err("an invalid URL must error");
        assert!(err.to_string().starts_with("Network Error: "), "got: {err}");
    }
}
