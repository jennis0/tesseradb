//! The session plane (R5): `POST /session/authorise`, `POST /session/revoke`, plus
//! `/healthz`/`/readyz`. Bearer auth is the operator-configured *session credential* — a shared
//! secret gating who may mint viewer sessions at all, distinct from the `auth_data` a caller
//! presents inside the request body to be evaluated against the bundle's dictionary.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{map_engine_error, map_join_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    // MVP client spec §3. The browser calls `/session/authorise` before it can call anything on
    // the viewer plane, so the seam has to cover this plane too or it covers nothing.
    let dev_cors = crate::cors::dev_layer(&state.dev_cors_origins);
    let router = Router::new()
        .route("/session/authorise", post(authorise))
        .route("/session/revoke", post(revoke))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state);
    match dev_cors {
        Some(layer) => router.layer(layer),
        None => router,
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

#[derive(Debug, Deserialize)]
struct AuthoriseReq {
    auth_data: String,
}

#[derive(Debug, Serialize)]
struct AuthoriseResp {
    token: String,
    token_id: u64,
    expires_at: u64,
}

async fn authorise(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<AuthoriseReq>,
) -> Result<Json<AuthoriseResp>, ApiError> {
    state.check_bearer(bearer_token(&headers), &state.session_credential)?;

    let auth_data = base64::engine::general_purpose::STANDARD
        .decode(&req.auth_data)
        .map_err(|e| ApiError::Contract(format!("auth_data is not valid base64: {e}")))?;

    // D-B: gated the same way as the viewer plane's closures — `admit()` sheds with 429
    // `backpressure` on either stage of the two-stage semaphore.
    let (gate_permits, _admission_us) = state.compute_gate.admit().await?;

    // D-A: `engine.authorise` resolves the credential's granted terms and unions them into a
    // fragment (I2) — on a fragment-cache miss this builds and writes the frozen fragment to
    // disk (file IO), and either way is CPU work with no `.await` of its own. Moved off the
    // reactor so a cold `authorise` cannot starve concurrent requests on this process's tokio
    // worker threads. Closure capture: `state` is a cloned `Arc<AppState>` (cheap; sound because
    // `Engine: Send + Sync`), `auth_data` is moved (owned `Vec<u8>`, only ever borrowed above),
    // `gate_permits` (D-B) moves in so both permits release only when this closure returns.
    let closure_state = Arc::clone(&state);
    let session = tokio::task::spawn_blocking(move || {
        let _gate_permits = gate_permits;
        closure_state.engine.authorise(&auth_data)
    })
    .await
    .map_err(map_join_error)?
    .map_err(map_engine_error)?;

    let resp = AuthoriseResp {
        token: session.token.clone(),
        token_id: session.token_id,
        expires_at: session.expires_at,
    };
    state.sessions.lock().insert(session);
    Ok(Json(resp))
}

#[derive(Debug, Deserialize)]
struct RevokeReq {
    token_id: u64,
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<RevokeReq>,
) -> Result<StatusCode, ApiError> {
    state.check_bearer(bearer_token(&headers), &state.session_credential)?;
    state.sessions.lock().revoke(req.token_id);
    // Task 5: drop the revoked session's row projections. The registry removal above is what makes
    // the session unusable (`authenticated_session` now returns `BadCredential`); this is memory
    // hygiene behind it, closing the Phase 1 deferral "revoke does not prune the projection cache".
    // Deliberately *after* the revoke, not before: a request that authenticated before this handler
    // ran can still re-publish its key, and doing the prune first would widen that window for no
    // benefit. See `RowProjectionCache::prune_token` for why the residue is bounded and benign.
    state.engine.prune_token(req.token_id);
    Ok(StatusCode::NO_CONTENT)
}
