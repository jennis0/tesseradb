//! The session plane: `POST /session/authorise`, `POST /session/revoke`, `/healthz` and
//! `/readyz`. It belongs to an integrator's backend and is never exposed to a browser. Its bearer
//! credential is an API key whose principal holds `authorise-as`, or the operator credential; it
//! mints a session for a local principal it names, or for an OIDC identity whose access token it
//! passes on. The session carries the target's terms and its `read` and `write`, and never its
//! `admin`, `authorise-as`, `read-all` or `write-all`. The operator credential alone may instead
//! name the terms the session holds, with `read` and nothing else, so a local operator reads as
//! any set of terms without a principal in the catalogue, or ask for a session of the superuser
//! itself, which holds `read-all` and reads every item.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use mosaica_catalogue::Permission;

use crate::auth::{self, Accepts, Caller};
use crate::error::ApiError;
use crate::health::{healthz, readyz};
use crate::state::{bearer_token, ApiJson, AppState, Principal};

pub fn router(state: Arc<AppState>) -> Router {
    // The development CORS list covers this plane, since a browser on a laptop authorises here
    // first. The production list never does: this plane's credential must never be held by a
    // browser, and `session_layer` reads only the development list.
    let dev_cors = crate::cors::session_layer(&state);
    let state_for_log = state.request_log.is_some().then(|| Arc::clone(&state));
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
    let router = match dev_cors {
        Some(layer) => router.layer(layer),
        None => router,
    };
    // Outermost, so a preflight the CORS layer answers is logged too.
    match state_for_log {
        Some(state) => router.layer(axum::middleware::from_fn_with_state(
            state,
            crate::request_log::session,
        )),
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

/// Exactly one of the four fields: the local principal to act as, the access token of the OIDC
/// identity to act as, or, with the operator credential alone, the terms the session holds or
/// `read_all: true` for a session of the superuser itself.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthoriseReq {
    principal: Option<String>,
    access_token: Option<String>,
    terms: Option<Vec<String>>,
    read_all: Option<bool>,
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
    let operator = matches!(minter.principal, Principal::Superuser);
    let operator_only = |what: &str| {
        ApiError::Forbidden(format!(
            "a session {what} is minted with the operator credential alone; name a `principal` \
             instead"
        ))
    };
    // The superuser's session over every item carries `read` and `read-all`; every other session
    // is minted through `authorise-as` and never carries `read-all`.
    let (target, minter) = match (req.principal, req.access_token, req.terms, req.read_all) {
        (Some(principal), None, None, None) => {
            (auth::named(&state.catalogue, &principal)?, Some(minter))
        }
        (None, None, Some(terms), None) if operator => {
            (auth::terms(&state.catalogue, terms), Some(minter))
        }
        (None, None, Some(_), None) => return Err(operator_only("for a set of terms")),
        (None, None, None, Some(true)) if operator => (auth::every_item(&state.catalogue), None),
        (None, None, None, Some(true)) => return Err(operator_only("reading every item")),
        (None, Some(token), None, None) => {
            let target = auth::access_token(&state, &token).await.map_err(|_| {
                ApiError::Contract(
                    "the access token was not accepted; send a current token from a declared \
                     provider"
                        .into(),
                )
            })?;
            (target, Some(minter))
        }
        _ => {
            return Err(ApiError::Contract(
                "send exactly one of `principal`, naming a local principal, `access_token`, an \
                 OIDC identity's access token, and, with the operator credential, `terms` or \
                 `read_all: true`"
                    .into(),
            ))
        }
    };
    let minted = auth::mint(&state, target, minter).await?;
    crate::request_log::note_token_id(minted.token_id);
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
    crate::request_log::note_token_id(req.token_id);
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
