//! The session plane: `POST /session/authorise`, `POST /session/revoke`, `/healthz` and
//! `/readyz`. It belongs to an integrator's backend and is never exposed to a browser. Its bearer
//! credential is an API key whose principal holds `authorise-as`, or the operator credential; it
//! mints a session for a local principal it names, or for an OIDC identity whose access token it
//! passes on. The session carries the target's terms and its `read` and `write`, and never its
//! `admin`, `authorise-as` or `bypass`. The operator credential alone may instead name the terms
//! the session holds, with `read` and nothing else, so a local operator reads as any set of terms
//! without a principal in the catalogue.

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

/// The session plane's authentication extractor: the operator credential, or an API key whose
/// principal holds `authorise-as`. Taken first after the state, so a caller without one is
/// refused before the body is read.
pub struct Integrator(pub Caller);

impl axum::extract::FromRequestParts<Arc<AppState>> for Integrator {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, ApiError> {
        let accepts = Accepts {
            operator: true,
            api_key: true,
            access_token: false,
        };
        let caller = auth::authenticate(state, bearer_token(&parts.headers), accepts).await?;
        caller.require(Permission::AuthoriseAs)?;
        Ok(Integrator(caller))
    }
}

/// Exactly one of the three fields: the local principal to act as, the access token of the OIDC
/// identity to act as, or, with the operator credential alone, the terms the session holds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoriseReq {
    principal: Option<String>,
    access_token: Option<String>,
    terms: Option<Vec<String>>,
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
    let target = match (req.principal, req.access_token, req.terms) {
        (Some(principal), None, None) => auth::named(&state.catalogue, &principal)?,
        (None, None, Some(terms)) => {
            if !matches!(minter.principal, Principal::Superuser) {
                return Err(ApiError::Forbidden(
                    "a session for a set of terms is minted with the operator credential alone; \
                     name a `principal` instead"
                        .into(),
                ));
            }
            auth::terms(&state.catalogue, terms)
        }
        (None, Some(token), None) => auth::access_token(&state, &token).await.map_err(|_| {
            ApiError::Contract(
                "the access token was not accepted; send a current token from a declared \
                 provider"
                    .into(),
            )
        })?,
        _ => {
            return Err(ApiError::Contract(
                "send exactly one of `principal`, naming a local principal, `access_token`, an \
                 OIDC identity's access token, and `terms`, with the operator credential"
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

/// Ends a session this caller's principal minted, or with the operator credential any session.
/// Any other `token_id` is answered the same way and changes nothing.
async fn revoke(
    State(state): State<Arc<AppState>>,
    Integrator(caller): Integrator,
    ApiJson(req): ApiJson<RevokeReq>,
) -> Result<StatusCode, ApiError> {
    let minter = match &caller.principal {
        Principal::Local(name) => Some(name.clone()),
        Principal::Superuser => None,
        Principal::Oidc { .. } => return Ok(StatusCode::NO_CONTENT),
    };
    state
        .end_sessions(|e| {
            e.session.token_id() == req.token_id
                && (minter.is_none() || e.minted_by.as_deref() == minter.as_deref())
        })
        .await;
    Ok(StatusCode::NO_CONTENT)
}
