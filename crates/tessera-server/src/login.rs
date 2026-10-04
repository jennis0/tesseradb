//! `POST /v1/login` and `POST /v1/logout` on the viewer plane. Login takes a password, an API key
//! or an OIDC access token and answers a session token for the principal it authenticates, which
//! must hold `read`. Logout ends the session of the token it is sent with.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::error::ApiError;
use crate::state::{ApiJson, AppState, ViewerSession};

/// One credential, named by its key.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LoginReq {
    Password(PasswordReq),
    ApiKey(String),
    AccessToken(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PasswordReq {
    principal: String,
    password: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct LoginResp {
    token: String,
    expires_at: u64,
}

pub(crate) async fn login(
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<LoginReq>,
) -> Result<Json<LoginResp>, ApiError> {
    let caller = match req {
        LoginReq::Password(p) => auth::password(&state, p.principal, p.password).await?,
        LoginReq::ApiKey(key) => auth::api_key(&state.catalogue, &key)?,
        LoginReq::AccessToken(token) => auth::access_token(&state, &token).await?,
    };
    let minted = auth::mint(&state, caller, None).await?;
    Ok(Json(LoginResp {
        token: minted.token,
        expires_at: minted.expires_at,
    }))
}

pub(crate) async fn logout(
    State(state): State<Arc<AppState>>,
    ViewerSession(session): ViewerSession,
) -> StatusCode {
    let token_id = session.token_id();
    state
        .end_sessions(|e| e.session.token_id() == token_id)
        .await;
    StatusCode::NO_CONTENT
}
