//! The session plane: `POST /session/authorise`, `POST /session/revoke`, `/healthz` and
//! `/readyz`. It belongs to an integrator's backend and is never exposed to a browser. Its bearer
//! credential is an API key whose principal holds `authorise-as`; the key mints a session for a
//! local principal it names, or for an OIDC identity whose access token it passes on. The session
//! carries the target's terms and its `read` and `write`, and never its `admin`, `authorise-as`
//! or `bypass`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use tessera_catalogue::Permission;

use crate::auth::{self, Accepts, Caller};
use crate::error::ApiError;
use crate::health::{healthz, readyz};
use crate::state::{bearer_token, ApiJson, AppState, Principal};

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

/// The session plane's authentication extractor: an API key whose principal holds
/// `authorise-as`. Taken first after the state, so a caller without one is refused before the
/// body is read.
pub struct Integrator(pub Caller);

impl axum::extract::FromRequestParts<Arc<AppState>> for Integrator {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, ApiError> {
        let accepts = Accepts {
            operator: false,
            api_key: true,
            access_token: false,
        };
        let caller = auth::authenticate(state, bearer_token(&parts.headers), accepts).await?;
        caller.require(Permission::AuthoriseAs)?;
        Ok(Integrator(caller))
    }
}

/// Exactly one of the two fields: the local principal to act as, or the access token of the OIDC
/// identity to act as.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoriseReq {
    principal: Option<String>,
    access_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AuthoriseResp {
    pub token: String,
    pub token_id: u64,
    pub expires_at: u64,
}

async fn authorise(
    State(state): State<Arc<AppState>>,
    Integrator(minter): Integrator,
    ApiJson(req): ApiJson<AuthoriseReq>,
) -> Result<Json<AuthoriseResp>, ApiError> {
    let target = match (req.principal, req.access_token) {
        (Some(principal), None) => auth::named(&state.catalogue, &principal)?,
        (None, Some(token)) => auth::access_token(&state, &token).await.map_err(|_| {
            ApiError::Contract(
                "the access token was not accepted; send a current token from a declared \
                 provider"
                    .into(),
            )
        })?,
        _ => {
            return Err(ApiError::Contract(
                "send exactly one of `principal`, naming a local principal, and `access_token`, \
                 an OIDC identity's access token"
                    .into(),
            ))
        }
    };
    let minted = auth::mint(&state, target, Some(minter)).await?;
    Ok(Json(AuthoriseResp {
        token: minted.token,
        token_id: minted.token_id,
        expires_at: minted.expires_at,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeReq {
    token_id: u64,
}

/// Ends a session this caller's principal minted. Any other `token_id` is answered the same way
/// and changes nothing.
async fn revoke(
    State(state): State<Arc<AppState>>,
    Integrator(caller): Integrator,
    ApiJson(req): ApiJson<RevokeReq>,
) -> Result<StatusCode, ApiError> {
    let minter = match &caller.principal {
        Principal::Local(name) => name.clone(),
        _ => return Ok(StatusCode::NO_CONTENT),
    };
    let ended = state.sessions.lock().end_where(|e| {
        e.session.token_id() == req.token_id && e.minted_by.as_deref() == Some(minter.as_str())
    });
    // The registry change above is what makes the session unusable; the prune is memory hygiene.
    state.prune_sessions(ended);
    Ok(StatusCode::NO_CONTENT)
}
