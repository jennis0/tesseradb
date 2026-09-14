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
    // The **development** seam covers this plane as well as the viewer plane: on a laptop the
    // browser calls `/session/authorise` before it can call anything on the viewer plane, so a
    // dev seam that missed this plane would cover nothing. `serve.cors_origins` — the production
    // list — deliberately does not reach here: this route is gated by the session credential,
    // which a browser must never hold (decision 0102). `session_layer` reads the dev list itself,
    // so this call site has no way to widen it.
    let dev_cors = crate::cors::session_layer(&state);
    let router = Router::new()
        .route("/session/authorise", post(authorise))
        .route("/session/revoke", post(revoke))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // The allocator's trim cadence (`crate::memory`). This plane builds the mask fragments,
        // which is where a new principal's anonymous growth arrives.
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            crate::memory::trim_after_response,
        ))
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

    // Gated the same way as the viewer plane's closures — `admit()` sheds with 429 `backpressure`
    // on either stage of the two-stage semaphore.
    let (gate_permits, _admission_us) = state.compute_gate.admit().await?;

    // `engine.authorise` resolves the credential's granted terms and unions them into a fragment
    // (I2: every aggregate must be computable from inside the viewer's own mask, so the mask is
    // materialised here once) — on a fragment-cache miss this builds and writes the frozen fragment
    // to disk (file IO), and either way is CPU work with no `.await` of its own. Moved off the
    // reactor so a cold `authorise` cannot starve concurrent requests on this process's tokio
    // worker threads. Closure capture: `state` is a cloned `Arc<AppState>` (cheap; sound because
    // `Engine: Send + Sync`), `auth_data` is moved (owned `Vec<u8>`, only ever borrowed above), and
    // `gate_permits` moves in so both permits release only when this closure returns.
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
    // Inserting is also what drives the registry's expiry sweep: authorisation is the only path
    // that grows the registry, so it is where the growth is bounded. The clock is read here, once
    // per request, rather than inside the lock — see `SessionRegistry`'s doc for what the sweep
    // costs, what bounds the pause, and why it is deliberately not a timer.
    let (_entry, expired) = state
        .sessions
        .lock()
        .insert(session, crate::state::now_secs());
    // The sweep's engine half, and the same call a revocation makes. The registry removal is what
    // makes a swept session unusable; this releases what the engine still holds under its token id.
    // Outside the lock above — the temporary guard is dropped at the end of that statement —
    // because a prune cancels a stage and walks five caches.
    for token_id in expired {
        state.engine.prune_token(token_id);
    }
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
    // Drop the revoked session's row projections. The registry removal above is what makes the
    // session unusable (`authenticated_session` now returns `BadCredential`); this is memory hygiene
    // behind it, so a revoked session's masks do not hold cache capacity against live ones.
    // Deliberately *after* the revoke, not before: a request that authenticated before this handler
    // ran can still re-publish its key, and doing the prune first would widen that window for no
    // benefit. See `RowProjectionCache::prune_token` for why the residue is bounded and benign.
    state.engine.prune_token(req.token_id);
    Ok(StatusCode::NO_CONTENT)
}
