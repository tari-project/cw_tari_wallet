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
/// (seconds since the Unix epoch).
///
/// Async; performs network I/O. Errors as `Network Error: …` if the URL is invalid or
/// the node cannot answer. Use [`estimate_height_at_time`] when offline.
#[frb]
pub async fn get_height_at_time(base_url: String, epoch_secs: u64) -> Result<u64> {
    height::height_at_time(&base_url, epoch_secs)
        .await
        .map_err(|e| WalletError::network(e.to_string()).into())
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

    #[tokio::test]
    async fn get_height_at_time_bad_url_is_network_error() {
        let err = get_height_at_time("not a url".to_string(), 0)
            .await
            .expect_err("an invalid URL must error");
        assert!(err.to_string().starts_with("Network Error: "), "got: {err}");
    }
}
