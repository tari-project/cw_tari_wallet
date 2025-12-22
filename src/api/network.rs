use anyhow::{Context, Result};
use std::str::FromStr;
use tari_common::configuration::Network;

pub(crate) fn parse_network(network: Option<String>) -> Result<Network> {
    network
        .as_deref()
        .map_or_else(|| Ok(Network::MainNet), Network::from_str)
        .context("failed to parse network")
}
