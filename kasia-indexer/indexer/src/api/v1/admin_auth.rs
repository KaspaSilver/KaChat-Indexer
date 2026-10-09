//! kachat-audits IDX-001: shared-secret guard for the maintenance/data routes
//! (`/export`, `/import-file`, `/personal/purge-all`, `/self-stash-gc-orphans`,
//! `/contextual-messages/import`). They share the public listener, so each request must
//! carry `x-internal-secret` equal to env `INTERNAL_PUSH_SECRET` (the same secret that
//! guards `/internal/push/*`). Fails closed: with no secret configured every request is
//! refused.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::OnceLock;

static SECRET: OnceLock<Option<String>> = OnceLock::new();

fn secret() -> Option<&'static str> {
    SECRET
        .get_or_init(|| {
            let s = std::env::var("INTERNAL_PUSH_SECRET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if s.is_none() {
                tracing::warn!(
                    "INTERNAL_PUSH_SECRET is not set: admin routes (/export, /import-file, \
                     /personal/purge-all, /self-stash-gc-orphans, /contextual-messages/import) \
                     refuse every request"
                );
            }
            s
        })
        .as_deref()
}

/// Logs the unset-secret warning at startup instead of on the first admin call.
pub fn init() {
    let _ = secret();
}

/// axum middleware: 401 unless `x-internal-secret` matches (constant time).
pub async fn require_internal_secret(req: Request, next: Next) -> Response {
    let authorized = secret().is_some_and(|s| {
        req.headers()
            .get("x-internal-secret")
            .is_some_and(|v| super::push::secret_matches(v.as_bytes(), s.as_bytes()))
    });
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(req).await
}
