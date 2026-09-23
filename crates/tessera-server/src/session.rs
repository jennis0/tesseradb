//! The session plane: `POST /session/authorise`, `POST /session/revoke`, `/healthz` and
//! `/readyz`. Its bearer token is the operator's session credential, which decides who may mint
//! viewer sessions; the `auth_data` in the body is what the bundle's dictionary evaluates.

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
    // The development CORS list covers this plane, since a browser on a laptop authorises here
    // first. The production list never does: this plane's credential must never be held by a
    // browser, and `session_layer` reads only the development list.
    let dev_cors = crate::cors::session_layer(&state);
    let router = Router::new()
        .route("/session/authorise", post(authorise))
        .route("/session/revoke", post(revoke))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        // Mask fragments for new principals are built here, so this plane trims the allocator too.
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

    // On a fragment-cache miss this builds the frozen fragment and writes it to disk.
    // The caller holds the session credential, so the plugin's reason for refusing `auth_data`
    // is theirs to read.
    let session = state
        .gated(move |state| {
            state.engine.authorise(&auth_data).map_err(|e| match e {
                tessera_engine::EngineError::Plugin(why) => ApiError::Contract(format!(
                    "the deployment's plugin refused auth_data: {why}"
                )),
                other => map_engine_error(other),
            })
        })
        .await?;

    let resp = AuthoriseResp {
        token: session.token().to_string(),
        token_id: session.token_id(),
        expires_at: session.expires_at(),
    };
    // Inserting sweeps expired sessions, since this is the only path that grows the registry.
    let expired = state
        .sessions
        .lock()
        .insert(session, crate::state::now_secs());
    // The registry removal already made the swept sessions unusable. Their engine caches are pruned
    // in one batch on a blocking thread, and the response does not wait for it.
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
    // The registry removal above is what makes the session unusable; this prune is memory hygiene
    // after it. Pruning first would widen the window in which a request that authenticated before
    // the revoke can re-publish its key.
    state.engine.prune_token(req.token_id);
    Ok(StatusCode::NO_CONTENT)
}
