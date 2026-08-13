use kaspa_wrpc_client::prelude::NetworkType;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize, Debug, Clone)]
pub struct IndexerConfig {
    #[serde(default = "default_kasia_indexer_db_root")]
    pub kasia_indexer_db_root: PathBuf,
    #[serde(default = "default_network_type")]
    pub network_type: NetworkType,
    pub kaspa_node_wborsh_url: Option<String>,
    #[serde(default = "default_periodic_processor_interval_secs")]
    pub periodic_processor_interval_secs: u64,
    pub apns_team_id: Option<String>,
    pub apns_key_id: Option<String>,
    pub apns_topic: Option<String>,
    pub apns_key_path: Option<PathBuf>,
    pub apns_key: Option<String>,
    #[serde(default = "default_apns_environment")]
    pub apns_environment: ApnsEnvironment,
    #[serde(default = "default_push_auth_mode")]
    pub push_auth_mode: PushAuthMode,
    // --- FCM (Firebase Cloud Messaging) for Android push ---
    // GCP/Firebase project id (e.g. "kachat-12345"); required to enable FCM delivery.
    pub fcm_project_id: Option<String>,
    // Path to the Firebase *service-account* JSON (Console → Project settings → Service
    // accounts → Generate new private key). Used for FCM HTTP v1 OAuth2. Mounted on the box,
    // like the APNs .p8. Prefer this over the inline form.
    pub fcm_service_account_path: Option<PathBuf>,
    // Inline alternative to fcm_service_account_path (the full service-account JSON as a string).
    pub fcm_service_account_json: Option<String>,
}

fn default_periodic_processor_interval_secs() -> u64 {
    30
}

fn default_network_type() -> NetworkType {
    NetworkType::Mainnet
}

fn default_kasia_indexer_db_root() -> PathBuf {
    std::env::home_dir().unwrap().join(".kasia-indexer")
}

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

fn default_apns_environment() -> ApnsEnvironment {
    ApnsEnvironment::Sandbox
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
