//! The session plane (R5): `POST /session/authorise`, `POST /session/revoke`, plus
//! `/healthz`/`/readyz`. Bearer auth is the operator-configured *session credential* — a shared
//! secret gating who may mint viewer sessions at all, distinct from the `auth_data` a caller
//! presents inside the request body to be evaluated against the bundle's dictionary.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{map_engine_error, ApiError};
use crate::health::{healthz, readyz};
use crate::state::{AppState, SessionCredential};

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
    _: SessionCredential,
    Json(req): Json<AuthoriseReq>,
) -> Result<Json<AuthoriseResp>, ApiError> {
    let auth_data = base64::engine::general_purpose::STANDARD
        .decode(&req.auth_data)
        .map_err(|e| ApiError::Contract(format!("auth_data is not valid base64: {e}")))?;

    // `engine.authorise` resolves the credential's granted terms and unions them into a fragment,
    // which on a fragment-cache miss builds and writes the frozen fragment to disk.
    let session = state
        .gated(move |state| state.engine.authorise(&auth_data).map_err(map_engine_error))
        .await?;

    let resp = AuthoriseResp {
        token: session.token().to_string(),
        token_id: session.token_id(),
        expires_at: session.expires_at(),
    };
    // Inserting is also what drives the registry's expiry sweep: authorisation is the only path
    // that grows the registry, so it is where the growth is bounded. The clock is read here, once
    // per request, rather than inside the lock — see `SessionRegistry`'s doc for what the sweep
    // costs, what bounds the pause, and why it is deliberately not a timer.
    let expired = state
        .sessions
        .lock()
        .insert(session, crate::state::now_secs());
    // The sweep's engine half, and the same removal a revocation makes. The registry removal is
    // what makes a swept session unusable; this releases what the engine still holds under its
    // token ids.
    //
    // **One batched call, on a blocking thread.** It walks five caches under five global mutexes,
    // which is the reactor's least welcome work and the request path's most contended lock — the
    // same reason the authorisation above runs on `spawn_blocking`. `Engine::prune_tokens` makes
    // the batch one pass per cache rather than one per session. The response does not wait for it:
    // the session is already inserted and the answer is already built, and a prune is memory
    // hygiene whose timing nothing observes.
    if !expired.is_empty() {
        let doomed: rustc_hash::FxHashSet<u64> = expired.into_iter().collect();
        let pruner = Arc::clone(&state);
        tokio::task::spawn_blocking(move || pruner.engine.prune_tokens(&doomed));
    }
    Ok(Json(resp))
}

#[derive(Debug, Deserialize)]
struct RevokeReq {
    token_id: u64,
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    _: SessionCredential,
    Json(req): Json<RevokeReq>,
) -> Result<StatusCode, ApiError> {
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
