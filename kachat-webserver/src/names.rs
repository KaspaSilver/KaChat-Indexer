//! `.kachat` names registry — testnet module (KACHAT_NAMES_INDEXER.md).
//!
//! This file is the module's **config + status + name-identity** surface:
//!   - loads the genesis manifest from `KACHAT_NAMES_MANIFEST` (a file path),
//!   - serves `GET /names/status` and `GET /names/manifest`,
//!   - validates a name and derives its `key = blake3(name)` (§B1).
//!
//! The registry itself — following the KachatGap / KachatName / KachatOffer covenant
//! spends in virtual-chain order, deriving each new state, and the full `/names/*`
//! lookup + marketplace + profile + push surface (§B–E) — is the covenant follower,
//! which lands next. Until it exists, the lookup endpoints answer 503, and the module
//! is **off** whenever no manifest is configured (exactly the spec's rollout state).

use crate::web_server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The genesis manifest handed over after the testnet-10 genesis
/// (`manifests/kachat-names-testnet-10.json` from the `kachat-domains` CLI). Only the
/// few fields the status endpoint surfaces are named; everything else is preserved as
/// raw JSON so `GET /names/manifest` round-trips the whole document unchanged.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamesManifest {
    #[serde(default)]
    pub network: Option<String>,
    #[serde(rename = "registryCovenantId", default)]
    pub registry_covenant_id: Option<String>,
    /// Registry v3: the price covenant (docs/KACHAT_NAMES_REGISTRY_V3.md).
    #[serde(rename = "priceCovenantId", default)]
    pub price_covenant_id: Option<String>,
    /// The genesis block of the registry. The live manifest nests the tx id + scan
    /// start here (`genesis.txid`, `genesis.scanFrom`).
    #[serde(default)]
    pub genesis: Option<NamesGenesis>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamesGenesis {
    #[serde(default)]
    pub txid: Option<String>,
    /// The block the follower should start scanning from (safe genesis checkpoint).
    #[serde(rename = "scanFrom", default)]
    pub scan_from: Option<String>,
}

/// The loaded module state. `manifest: None` means the module is OFF (no manifest, or
/// it did not parse) — the registry endpoints answer 503 and status reports `on: false`.
#[derive(Clone, Default)]
pub struct NamesState {
    pub manifest: Option<Arc<NamesManifest>>,
    /// The manifest verbatim, for `GET /names/manifest`.
    pub raw: Option<Arc<serde_json::Value>>,
    pub path: Option<String>,
}

impl NamesState {
    /// Load from `KACHAT_NAMES_MANIFEST` (a file path). Absent / blank / unreadable /
    /// unparseable all mean the module stays off — nothing here ever fails startup.
    pub fn from_env() -> Self {
        let Some(path) = std::env::var("KACHAT_NAMES_MANIFEST")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(raw) => {
                    let manifest = serde_json::from_value::<NamesManifest>(raw.clone()).ok();
                    if manifest.is_none() {
                        tracing::warn!(%path, "[names] manifest did not parse; module off");
                    } else {
                        tracing::info!(%path, "[names] module on (manifest loaded)");
                    }
                    Self {
                        manifest: manifest.map(Arc::new),
                        raw: Some(Arc::new(raw)),
                        path: Some(path),
                    }
                }
                Err(e) => {
                    tracing::warn!(%path, error = %e, "[names] manifest is not valid JSON; module off");
                    Self { path: Some(path), ..Self::default() }
                }
            },
            Err(e) => {
                tracing::warn!(%path, error = %e, "[names] manifest unreadable; module off");
                Self { path: Some(path), ..Self::default() }
            }
        }
    }

    pub fn is_on(&self) -> bool {
        self.manifest.is_some()
    }
}

/// Normalise a user-supplied name: trim, lowercase, drop an optional `.kachat` suffix,
/// then validate. Returns the canonical name, or `None` if it breaks the charset/length
/// rules (§B1).
pub fn normalize_name(input: &str) -> Option<String> {
    let lowered = input.trim().to_ascii_lowercase();
    let name = lowered.strip_suffix(".kachat").unwrap_or(&lowered).to_string();
    is_valid_name(&name).then_some(name)
}

/// §B1: `a-z 0-9 -`, 1..32 chars, no hyphen at either end.
pub fn is_valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 32 {
        return false;
    }
    if b[0] == b'-' || b[b.len() - 1] == b'-' {
        return false;
    }
    b.iter()
        .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// §B1: `key = blake3(name)` over the ASCII bytes of the (already-validated) name, hex.
pub fn name_key_hex(name: &str) -> String {
    hex::encode(blake3::hash(name.as_bytes()).as_bytes())
}

// --------------------------------------------------------------------- API ----

/// GET /names/status — `{network, registryCovenantId, genesisTxId, indexedDaa, synced}`
/// plus `on`. Until the covenant follower exists, `indexedDaa` is 0 and `synced` is
/// false; `on` reflects whether a manifest is loaded.
///
/// `registryCovenantId` is the clients' switch: iOS, Android and Desktop stop walking the
/// chain and route every `/names/*` lookup here as soon as it matches their manifest. So it
/// is withheld (null) until the follower can actually answer those lookups; the manifest's
/// id is reported separately as `manifestRegistryCovenantId` for the panel.
pub async fn handle_names_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let n = &state.names;
    let manifest_registry = n.manifest.as_ref().and_then(|m| m.registry_covenant_id.clone());
    // Synced = kachat-names-follower caught up, self-tested clean, heartbeat fresh, and
    // following this same registry (see names_api::follower_status).
    let follower = match &manifest_registry {
        Some(r) => crate::names_api::follower_status(&state.scheduled_pool, r).await,
        None => None,
    };
    let manifest_price = n.manifest.as_ref().and_then(|m| m.price_covenant_id.clone());
    let synced = follower.as_ref().is_some_and(|f| {
        f.synced && crate::names_api::same_price_covenant(manifest_price.as_deref(), f.price_covenant_id.as_deref())
    });
    let indexed_daa = follower.as_ref().map(|f| f.indexed_daa).unwrap_or(0);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "network": n.manifest.as_ref().and_then(|m| m.network.clone()),
            "registryCovenantId": if synced { manifest_registry.clone() } else { None },
            "manifestRegistryCovenantId": manifest_registry,
            // Registry v3: reported only once synced, exactly like registryCovenantId (the
            // app uses this indexer only when both match its manifest).
            "priceCovenantId": if synced { manifest_price.clone() } else { None },
            "manifestPriceCovenantId": manifest_price,
            "genesisTxId": n.manifest.as_ref().and_then(|m| m.genesis.as_ref()).and_then(|g| g.txid.clone()),
            "scanFrom": n.manifest.as_ref().and_then(|m| m.genesis.as_ref()).and_then(|g| g.scan_from.clone()),
            "indexedDaa": indexed_daa,
            "synced": synced,
            "on": n.is_on(),
            "manifestPath": n.path,
            // KACHAT_NAMES_PRUNED_START.md §2: e.g. "start_block_pruned" when the node no
            // longer has the block the follower must start from (it never recovers by itself).
            "error": follower.as_ref().and_then(|f| f.fatal_reason.clone()),
            "startBlock": follower.as_ref().and_then(|f| f.start_block.as_ref().map(hex::encode)),
        })),
    )
}

/// GET /names/manifest — the manifest the module runs with, verbatim; 503 when off.
pub async fn handle_names_manifest(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match &state.names.raw {
        Some(raw) => (StatusCode::OK, Json((**raw).clone())).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no_manifest", "message": "names module is off" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules() {
        assert!(is_valid_name("alice"));
        assert!(is_valid_name("a"));
        assert!(is_valid_name("a-b-1"));
        assert!(is_valid_name(&"a".repeat(32)));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name(&"a".repeat(33)));
        assert!(!is_valid_name("-alice"));
        assert!(!is_valid_name("alice-"));
        assert!(!is_valid_name("Alice")); // uppercase rejected
        assert!(!is_valid_name("al.ce"));
    }

    #[test]
    fn normalises_suffix_and_case() {
        assert_eq!(normalize_name("  Alice.kachat \n").as_deref(), Some("alice"));
        assert_eq!(normalize_name("BOB").as_deref(), Some("bob"));
        assert_eq!(normalize_name("bad_name"), None);
    }

    #[test]
    fn parses_the_live_testnet_manifest_shape() {
        // The real testnet-10 manifest shape (kachat-domains): network + registryCovenantId at the
        // top level, genesis nested with txid + scanFrom.
        let json = r#"{
            "network": "testnet-10",
            "registryCovenantId": "9444187f09a3e77450e125d448b21eb79b3c54b692a5b3f3e8af38343b9a7a51",
            "genesis": {
                "txid": "cba68dd1b07f374410270f1e609a3e71deaf42d3bd3b5849ac9b0e9cc687f45f",
                "scanFrom": "167f1ce5be24510c328c3a286228bff99d0fde505aa105fc990708ba39edd935",
                "covenantId": "9444187f09a3e77450e125d448b21eb79b3c54b692a5b3f3e8af38343b9a7a51"
            },
            "artifacts": {}, "params": {}, "status": "deployed"
        }"#;
        let m: NamesManifest = serde_json::from_str(json).expect("manifest parses");
        assert_eq!(m.network.as_deref(), Some("testnet-10"));
        assert_eq!(
            m.registry_covenant_id.as_deref(),
            Some("9444187f09a3e77450e125d448b21eb79b3c54b692a5b3f3e8af38343b9a7a51")
        );
        let g = m.genesis.expect("genesis present");
        assert_eq!(g.txid.as_deref(), Some("cba68dd1b07f374410270f1e609a3e71deaf42d3bd3b5849ac9b0e9cc687f45f"));
        assert!(g.scan_from.is_some());
    }

    #[test]
    fn key_is_blake3_of_name() {
        // Cross-checkable: blake3 of the ASCII name, hex. Pinned so a hashing drift is caught.
        assert_eq!(name_key_hex("alice"), hex::encode(blake3::hash(b"alice").as_bytes()));
        assert_eq!(name_key_hex("alice").len(), 64);
    }
}
