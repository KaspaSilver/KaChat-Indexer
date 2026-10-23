use kaspa_wrpc_client::prelude::NetworkType;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Debug, Clone)]
pub struct IndexerConfig {
    #[serde(default = "default_kasia_indexer_db_root")]
    pub kasia_indexer_db_root: PathBuf,
    // Accepts "testnet-10" as well as "testnet": the network suffix (10) is applied
    // elsewhere, and stacks pass the full network id (NETWORK_TYPE=testnet-10).
    #[serde(default = "default_network_type", deserialize_with = "network_type_lenient")]
    pub network_type: NetworkType,
    pub kaspa_node_wborsh_url: Option<String>,
    #[serde(default = "default_periodic_processor_interval_secs")]
    pub periodic_processor_interval_secs: u64,
    #[serde(default = "default_push_auth_mode")]
    pub push_auth_mode: PushAuthMode,
    // The Apple (APNS_*) and Android (FCM_*) push settings were removed 2026-10-22 with their
    // transports: Web Push only. Old values in the environment are ignored.
}

fn default_periodic_processor_interval_secs() -> u64 {
    30
}

fn default_network_type() -> NetworkType {
    NetworkType::Mainnet
}

/// `mainnet` / `testnet` / `devnet` / `simnet`, optionally with a `-<suffix>` (e.g.
/// `testnet-10`) which is ignored here.
fn network_type_lenient<'de, D: serde::Deserializer<'de>>(d: D) -> Result<NetworkType, D::Error> {
    let raw = String::deserialize(d)?;
    parse_network_type(&raw).ok_or_else(|| {
        serde::de::Error::custom(format!("unknown network `{raw}`, expected mainnet, testnet(-10), devnet or simnet"))
    })
}

fn parse_network_type(raw: &str) -> Option<NetworkType> {
    let base = raw.trim().to_ascii_lowercase();
    let base = base.split('-').next().unwrap_or("");
    Some(match base {
        "mainnet" => NetworkType::Mainnet,
        "testnet" => NetworkType::Testnet,
        "devnet" => NetworkType::Devnet,
        "simnet" => NetworkType::Simnet,
        _ => return None,
    })
}

#[cfg(test)]
mod network_type_tests {
    use super::*;

    #[test]
    fn accepts_the_full_network_id() {
        assert_eq!(parse_network_type("testnet-10"), Some(NetworkType::Testnet));
        assert_eq!(parse_network_type("testnet"), Some(NetworkType::Testnet));
        assert_eq!(parse_network_type("Mainnet"), Some(NetworkType::Mainnet));
        assert_eq!(parse_network_type("bogus"), None);
    }
}

fn default_kasia_indexer_db_root() -> PathBuf {
    std::env::home_dir().unwrap().join(".kasia-indexer")
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PushAuthMode {
    Legacy,
    Mixed,
    Strict,
}

fn default_push_auth_mode() -> PushAuthMode {
    PushAuthMode::Mixed
}

pub fn get_indexer_config() -> anyhow::Result<IndexerConfig> {
    Ok(envy::from_env::<IndexerConfig>()?)
}
