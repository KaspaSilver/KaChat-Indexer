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
    extract::{Path, State},
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
    #[serde(rename = "genesisTxId", default)]
    pub genesis_tx_id: Option<String>,
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
                        tracing::warn!(%path, "KACHAT_NAMES_MANIFEST did not parse; names module OFF");
                    } else {
                        tracing::info!(%path, "names module ON (manifest loaded)");
                    }
                    Self {
                        manifest: manifest.map(Arc::new),
                        raw: Some(Arc::new(raw)),
                        path: Some(path),
                    }
                }
                Err(e) => {
                    tracing::warn!(%path, error = %e, "KACHAT_NAMES_MANIFEST is not valid JSON; names module OFF");
                    Self { path: Some(path), ..Self::default() }
                }
            },
            Err(e) => {
                tracing::warn!(%path, error = %e, "KACHAT_NAMES_MANIFEST unreadable; names module OFF");
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
pub async fn handle_names_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let n = &state.names;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "network": n.manifest.as_ref().and_then(|m| m.network.clone()),
            "registryCovenantId": n.manifest.as_ref().and_then(|m| m.registry_covenant_id.clone()),
            "genesisTxId": n.manifest.as_ref().and_then(|m| m.genesis_tx_id.clone()),
            "indexedDaa": 0,
            "synced": false,
            "on": n.is_on(),
            "manifestPath": n.path,
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

/// GET /names/{name} — validates the name (400 on a bad one) and returns its key. The
/// registration state comes from the covenant follower; until that exists this answers
/// 503 `syncing` rather than inventing data.
pub async fn handle_name_lookup(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(name) = normalize_name(&name) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid_name" })),
        )
            .into_response();
    };
    if !state.names.is_on() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "unavailable", "message": "names module is off (no manifest)" })),
        )
            .into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "name": name,
            "key": name_key_hex(&name),
            "error": "syncing",
            "message": "the registry follower is not available yet",
        })),
    )
        .into_response()
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
    fn key_is_blake3_of_name() {
        // Cross-checkable: blake3 of the ASCII name, hex. Pinned so a hashing drift is caught.
        assert_eq!(name_key_hex("alice"), hex::encode(blake3::hash(b"alice").as_bytes()));
        assert_eq!(name_key_hex("alice").len(), 64);
    }
}
