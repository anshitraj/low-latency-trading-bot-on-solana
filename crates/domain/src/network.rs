use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Cluster a record/transaction belongs to. Every persisted trade row carries
/// this so devnet and mainnet data can never be mixed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mock,
    Localnet,
    Devnet,
    Testnet,
    Mainnet,
}

pub const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
pub const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";
pub const TESTNET_GENESIS: &str = "4uhcVJyU9pJkvQyS88uRDiswHXSCkY3zQawwpjk2NsNY";

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Network::Mock => "mock",
            Network::Localnet => "localnet",
            Network::Devnet => "devnet",
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        }
    }
    pub fn is_mainnet(self) -> bool {
        matches!(self, Network::Mainnet)
    }
    /// Verify that an RPC endpoint's genesis hash matches the configured network.
    /// Localnet accepts any hash that is *not* a public cluster's.
    pub fn verify_genesis(self, genesis_hash: &str) -> Result<(), String> {
        let expected = match self {
            Network::Mainnet => Some(MAINNET_GENESIS),
            Network::Devnet => Some(DEVNET_GENESIS),
            Network::Testnet => Some(TESTNET_GENESIS),
            Network::Localnet | Network::Mock => None,
        };
        match expected {
            Some(e) if e == genesis_hash => Ok(()),
            Some(e) => Err(format!(
                "genesis mismatch for {}: expected {e}, rpc reports {genesis_hash}",
                self.as_str()
            )),
            None => {
                if [MAINNET_GENESIS, DEVNET_GENESIS, TESTNET_GENESIS].contains(&genesis_hash) {
                    Err(format!(
                        "configured {} but rpc is a public cluster ({genesis_hash})",
                        self.as_str()
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Network {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "mock" => Ok(Network::Mock),
            "localnet" | "local" => Ok(Network::Localnet),
            "devnet" => Ok(Network::Devnet),
            "testnet" => Ok(Network::Testnet),
            "mainnet" | "mainnet-beta" => Ok(Network::Mainnet),
            other => Err(format!("unknown network {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn genesis_guard() {
        assert!(Network::Mainnet.verify_genesis(MAINNET_GENESIS).is_ok());
        assert!(Network::Mainnet.verify_genesis(DEVNET_GENESIS).is_err());
        assert!(Network::Devnet.verify_genesis(MAINNET_GENESIS).is_err());
        assert!(Network::Localnet.verify_genesis(MAINNET_GENESIS).is_err());
        assert!(Network::Localnet.verify_genesis("SomeLocalHash111").is_ok());
        assert_eq!("mainnet-beta".parse::<Network>().unwrap(), Network::Mainnet);
    }
}
