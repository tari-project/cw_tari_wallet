use anyhow::{anyhow, Error};
use std::str::FromStr;

use flutter_rust_bridge::frb;
use tari_common::configuration::Network;

#[frb]
#[derive(Clone, PartialEq, Eq, Copy, Debug)]
pub enum TariNetwork {
    MainNet,
    StageNet,
    NextNet,
    LocalNet,
    Igor,
    Esmeralda,
}

impl FromStr for TariNetwork {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mainnet" => Ok(TariNetwork::MainNet),
            "stagenet" => Ok(TariNetwork::StageNet),
            "nextnet" => Ok(TariNetwork::NextNet),
            "localnet" => Ok(TariNetwork::LocalNet),
            "igor" => Ok(TariNetwork::Igor),
            "esmeralda" | "esme" => Ok(TariNetwork::Esmeralda),
            invalid => Err(anyhow!("Invalid network option: {invalid}")),
        }
    }
}

impl From<TariNetwork> for Network {
    fn from(network: TariNetwork) -> Self {
        match network {
            TariNetwork::MainNet => Network::MainNet,
            TariNetwork::StageNet => Network::StageNet,
            TariNetwork::NextNet => Network::NextNet,
            TariNetwork::LocalNet => Network::LocalNet,
            TariNetwork::Igor => Network::Igor,
            TariNetwork::Esmeralda => Network::Esmeralda,
        }
    }
}

pub(crate) fn parse_network(network: Option<TariNetwork>) -> Network {
    network.map_or_else(|| Network::MainNet, Into::into)
}
