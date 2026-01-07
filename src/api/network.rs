use flutter_rust_bridge::frb;
use tari_common::configuration::Network;

#[frb]
#[derive(Clone, PartialEq, Eq, Copy)]
pub enum TariNetwork {
    MainNet,
    StageNet,
    NextNet,
    LocalNet,
    Igor,
    Esmeralda,
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
