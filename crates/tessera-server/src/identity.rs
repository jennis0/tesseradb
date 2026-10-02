//! The catalogue's verbs on the control plane: principals, passwords, API keys, groups, grants,
//! OIDC providers, and the sessions they authorised. Every route needs `admin`, checked by the
//! control router.
//!
//! A change commits to the catalogue first, then ends every session it affected, and answers
//! how many it ended. A read answers names, terms and permissions; it never answers a password
//! hash or an API key's secret, which is shown once, when the key is created.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use tessera_catalogue::{
    Affected, ApiKeyInfo, ClaimRule, Error as CatalogueError, Grantee, GroupInfo, Permission,
    PermissionSet, PrincipalInfo, PrincipalKind, Provider, ProviderInfo, RoleMapping,
};

use crate::error::ApiError;
use crate::state::{ApiJson, ApiQuery, AppState, Principal, SessionEntry};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/control/principals",
            get(list_principals).post(create_principal),
        )
        .route(
            "/control/principals/{name}",
            get(show_principal)
                .patch(change_principal)
                .delete(delete_principal),
        )
        .route(
            "/control/principals/{name}/password",
            put(set_password).delete(clear_password),
        )
        .route(
            "/control/principals/{name}/keys",
            get(list_keys).post(create_key),
        )
        .route("/control/keys/{prefix}", delete(revoke_key))
        .route("/control/groups", get(list_groups).post(create_group))
        .route(
            "/control/groups/{name}",
            get(show_group).delete(delete_group),
        )
        .route(
            "/control/groups/{name}/members/{principal}",
            put(add_member).delete(remove_member),
        )
        .route("/control/grants", post(grant))
        .route("/control/grants/revoke", post(revoke))
        .route("/control/providers", get(list_providers))
        .route(
            "/control/providers/{name}",
            get(show_provider).put(put_provider).delete(drop_provider),
        )
        .route("/control/sessions", get(list_sessions))
        .route("/control/sessions/end", post(end_sessions))
}

/// A catalogue refusal as the API answers it. A storage failure is logged and answered without
/// its text, which can name the catalogue's path.
fn refusal(e: CatalogueError) -> ApiError {
    match e {
        CatalogueError::Invalid(_) | CatalogueError::InsecureJwks { .. } => {
            ApiError::Contract(e.to_string())
        }
        CatalogueError::NotFound { .. } => ApiError::Unknown(e.to_string()),
        CatalogueError::Exists { .. }
        | CatalogueError::ReadOnly { .. }
        | CatalogueError::DeclaredTwice { .. } => ApiError::Conflict(e.to_string()),
        other => {
            tracing::error!(error = %other, "the catalogue could not be changed");
            ApiError::FailClosed(
                "the catalogue could not be changed, and nothing was changed".into(),
            )
        }
    }
}

#[derive(Serialize)]
struct Change {
    sessions_ended: usize,
}

/// Runs a catalogue change off the async threads, then ends the sessions it affected.
async fn change(
    state: &Arc<AppState>,
    f: impl FnOnce(&tessera_catalogue::Catalogue) -> Result<Affected, CatalogueError> + Send + 'static,
) -> Result<Json<Change>, ApiError> {
    let affected = state
        .blocking(move |state| f(&state.catalogue).map_err(refusal))
        .await?;
    Ok(Json(Change {
        sessions_ended: state.end_affected(&affected).await,
    }))
}

fn permissions(p: PermissionSet) -> Vec<&'static str> {
    p.iter().map(Permission::as_str).collect()
}

fn parse_permissions(names: &[String]) -> Result<PermissionSet, ApiError> {
    names
        .iter()
        .map(|n| Permission::parse(n).map_err(refusal))
        .collect()
}

fn principal_json(p: PrincipalInfo) -> Value {
    json!({
        "name": p.name,
        "kind": p.kind.as_str(),
        "disabled": p.disabled,
        "bypass": p.bypass,
        "has_password": p.has_password,
        "terms": p.terms,
        "permissions": permissions(p.permissions),
        "groups": p.groups,
    })
}

fn group_json(g: GroupInfo) -> Value {
    json!({
        "name": g.name,
        "terms": g.terms,
        "permissions": permissions(g.permissions),
        "members": g.members,
    })
}

fn key_json(k: ApiKeyInfo) -> Value {
    json!({
        "prefix": k.prefix,
        "principal": k.principal,
        "created_at": k.created_at,
        "expires_at": k.expires_at,
        "permissions": k.permissions.map(permissions),
    })
}

fn provider_json(p: ProviderInfo) -> Value {
    let ProviderInfo {
        provider,
        read_only,
    } = p;
    json!({
        "name": provider.name,
        "issuer": provider.issuer,
        "audience": provider.audience,
        "jwks_url": provider.jwks_url,
        "claim_rules": provider
            .rules
            .iter()
            .map(|r| json!({ "claim": r.claim, "template": r.template }))
            .collect::<Vec<_>>(),
        "role_mappings": provider
            .role_mappings
            .iter()
            .map(|m| json!({ "claim": m.claim, "value": m.value, "group": m.group }))
            .collect::<Vec<_>>(),
        "read_only": read_only,
    })
}

fn session_json(e: &SessionEntry) -> Value {
    let (principal, provider, subject) = match &e.principal {
        Principal::Local(name) => (Some(name.as_str()), None, None),
        Principal::Oidc {
            provider, subject, ..
        } => (None, Some(provider.as_str()), Some(subject.as_str())),
        Principal::Superuser => (None, None, None),
    };
    json!({
        "token_id": e.session.token_id(),
        "principal": principal,
        "provider": provider,
        "subject": subject,
        "api_key": e.api_key,
        "minted_by": e.minted_by,
        "permissions": permissions(e.permissions),
        "created_at": e.created_at,
        "expires_at": e.expires_at,
    })
}

fn unknown(what: &str, name: &str) -> ApiError {
    ApiError::Unknown(format!(
        "there is no {what} named `{name}`; list the {what}s to find the name"
    ))
}

async fn list_principals(State(state): State<Arc<AppState>>) -> Json<Value> {
    let principals: Vec<Value> = state
        .catalogue
        .principals()
        .into_iter()
        .map(principal_json)
        .collect();
    Json(json!({ "principals": principals }))
}

async fn show_principal(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state
        .catalogue
        .principal(&name)
        .map(|p| Json(principal_json(p)))
        .ok_or_else(|| unknown("principal", &name))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreatePrincipal {
    name: String,
    kind: String,
}

async fn create_principal(
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<CreatePrincipal>,
) -> Result<Json<Change>, ApiError> {
    let kind = PrincipalKind::parse(&req.kind).ok_or_else(|| {
        ApiError::Contract(format!(
            "`{}` is not a kind of principal; write `person` or `service`",
            req.kind
        ))
    })?;
    change(&state, move |c| c.create_principal(&req.name, kind)).await
}

/// Each field present is applied, `disabled` first.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePrincipal {
    disabled: Option<bool>,
    bypass: Option<bool>,
}

async fn change_principal(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    ApiJson(req): ApiJson<ChangePrincipal>,
) -> Result<Json<Change>, ApiError> {
    if req.disabled.is_none() && req.bypass.is_none() {
        return Err(ApiError::Contract(
            "the request changes nothing; send `disabled`, `bypass` or both".into(),
        ));
    }
    change(&state, move |c| {
        let mut affected = Affected::default();
        if let Some(disabled) = req.disabled {
            let a = if disabled {
                c.disable_principal(&name)?
            } else {
                c.enable_principal(&name)?
            };
            merge(&mut affected, a);
        }
        if let Some(bypass) = req.bypass {
            merge(&mut affected, c.set_bypass(&name, bypass)?);
        }
        Ok(affected)
    })
    .await
}

fn merge(into: &mut Affected, from: Affected) {
    into.principals.extend(from.principals);
    into.api_keys.extend(from.api_keys);
    into.providers.extend(from.providers);
    into.generation = into.generation.max(from.generation);
}

async fn delete_principal(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.delete_principal(&name)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPassword {
    password: String,
}

async fn set_password(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    ApiJson(req): ApiJson<SetPassword>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.set_password(&name, &req.password)).await
}

async fn clear_password(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.clear_password(&name)).await
}

async fn list_keys(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if state.catalogue.principal(&name).is_none() {
        return Err(unknown("principal", &name));
    }
    let keys: Vec<Value> = state
        .catalogue
        .api_keys(&name)
        .into_iter()
        .map(key_json)
        .collect();
    Ok(Json(json!({ "keys": keys })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateKey {
    /// Seconds since the Unix epoch.
    expires_at: Option<u64>,
    /// The key's own permissions, narrower than its principal's.
    permissions: Option<Vec<String>>,
}

async fn create_key(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    ApiJson(req): ApiJson<CreateKey>,
) -> Result<Json<Value>, ApiError> {
    let narrow = req.permissions.as_deref().map(parse_permissions).transpose()?;
    let issued = state
        .blocking(move |state| {
            state
                .catalogue
                .create_api_key(&name, req.expires_at, narrow)
                .map_err(refusal)
        })
        .await?
        .0;
    Ok(Json(json!({ "prefix": issued.prefix, "key": issued.key })))
}

async fn revoke_key(
    State(state): State<Arc<AppState>>,
    Path(prefix): Path<String>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.revoke_api_key(&prefix)).await
}

async fn list_groups(State(state): State<Arc<AppState>>) -> Json<Value> {
    let groups: Vec<Value> = state
        .catalogue
        .groups()
        .into_iter()
        .map(group_json)
        .collect();
    Json(json!({ "groups": groups }))
}

async fn show_group(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state
        .catalogue
        .group(&name)
        .map(|g| Json(group_json(g)))
        .ok_or_else(|| unknown("group", &name))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateGroup {
    name: String,
}

async fn create_group(
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<CreateGroup>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.create_group(&req.name)).await
}

async fn delete_group(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.delete_group(&name)).await
}

async fn add_member(
    State(state): State<Arc<AppState>>,
    Path((group, principal)): Path<(String, String)>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.add_member(&group, &principal)).await
}

async fn remove_member(
    State(state): State<Arc<AppState>>,
    Path((group, principal)): Path<(String, String)>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.remove_member(&group, &principal)).await
}

/// One grant: to exactly one of `principal` and `group`, of exactly one of `term` and
/// `permission`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantReq {
    principal: Option<String>,
    group: Option<String>,
    term: Option<String>,
    permission: Option<String>,
}

enum Granted {
    Term(String),
    Permission(Permission),
}

fn grant_parts(req: GrantReq) -> Result<(bool, String, Granted), ApiError> {
    let (is_group, name) = match (req.principal, req.group) {
        (Some(p), None) => (false, p),
        (None, Some(g)) => (true, g),
        _ => {
            return Err(ApiError::Contract(
                "send exactly one of `principal` and `group`".into(),
            ))
        }
    };
    let what = match (req.term, req.permission) {
        (Some(t), None) => Granted::Term(t),
        (None, Some(p)) => Granted::Permission(Permission::parse(&p).map_err(refusal)?),
        _ => {
            return Err(ApiError::Contract(
                "send exactly one of `term` and `permission`".into(),
            ))
        }
    };
    Ok((is_group, name, what))
}

async fn grant(
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<GrantReq>,
) -> Result<Json<Change>, ApiError> {
    grant_or_revoke(state, req, true).await
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<GrantReq>,
) -> Result<Json<Change>, ApiError> {
    grant_or_revoke(state, req, false).await
}

async fn grant_or_revoke(
    state: Arc<AppState>,
    req: GrantReq,
    give: bool,
) -> Result<Json<Change>, ApiError> {
    let (is_group, name, what) = grant_parts(req)?;
    change(&state, move |c| {
        let who = if is_group {
            Grantee::Group(&name)
        } else {
            Grantee::Principal(&name)
        };
        match (what, give) {
            (Granted::Term(t), true) => c.grant_term(who, &t),
            (Granted::Term(t), false) => c.revoke_term(who, &t),
            (Granted::Permission(p), true) => c.grant_permission(who, p),
            (Granted::Permission(p), false) => c.revoke_permission(who, p),
        }
    })
    .await
}

async fn list_providers(State(state): State<Arc<AppState>>) -> Json<Value> {
    let providers: Vec<Value> = state
        .catalogue
        .providers()
        .into_iter()
        .map(provider_json)
        .collect();
    Json(json!({ "providers": providers }))
}

async fn show_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    state
        .catalogue
        .provider(&name)
        .map(|p| Json(provider_json(p)))
        .ok_or_else(|| unknown("provider", &name))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimRuleReq {
    claim: String,
    template: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleMappingReq {
    claim: String,
    value: String,
    group: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderReq {
    issuer: String,
    audience: String,
    jwks_url: String,
    #[serde(default)]
    claim_rules: Vec<ClaimRuleReq>,
    #[serde(default)]
    role_mappings: Vec<RoleMappingReq>,
}

/// Declares the provider named in the path, or replaces it whole where it exists.
async fn put_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    ApiJson(req): ApiJson<ProviderReq>,
) -> Result<Json<Change>, ApiError> {
    let provider = Provider {
        name,
        issuer: req.issuer,
        audience: req.audience,
        jwks_url: req.jwks_url,
        rules: req
            .claim_rules
            .into_iter()
            .map(|r| ClaimRule {
                claim: r.claim,
                template: r.template,
            })
            .collect(),
        role_mappings: req
            .role_mappings
            .into_iter()
            .map(|m| RoleMapping {
                claim: m.claim,
                value: m.value,
                group: m.group,
            })
            .collect(),
    };
    change(&state, move |c| {
        if c.provider(&provider.name).is_some() {
            c.update_provider(&provider)
        } else {
            c.create_provider(&provider)
        }
    })
    .await
}

async fn drop_provider(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Result<Json<Change>, ApiError> {
    change(&state, move |c| c.drop_provider(&name)).await
}

/// At most one: a local principal's sessions, or those authorised through a provider.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionsQuery {
    principal: Option<String>,
    provider: Option<String>,
}

/// Which sessions a filter selects: those of the local principal, including those minted for it
/// through `authorise-as`, or those authorised through the provider.
fn selects(principal: &Option<String>, provider: &Option<String>, e: &SessionEntry) -> bool {
    let p = principal.as_deref().map(str::trim);
    let o = provider.as_deref().map(str::trim);
    match (&e.principal, p, o) {
        (_, None, None) => true,
        (Principal::Local(name), Some(want), None) => name == want,
        (Principal::Oidc { provider, .. }, None, Some(want)) => provider == want,
        _ => false,
    }
}

async fn list_sessions(
    State(state): State<Arc<AppState>>,
    ApiQuery(q): ApiQuery<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.principal.is_some() && q.provider.is_some() {
        return Err(ApiError::Contract(
            "send at most one of `principal` and `provider`".into(),
        ));
    }
    let sessions: Vec<Value> = state
        .sessions
        .lock()
        .list(|e| selects(&q.principal, &q.provider, e))
        .iter()
        .map(|e| session_json(e))
        .collect();
    Ok(Json(json!({ "sessions": sessions })))
}

/// Exactly one of the three.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EndReq {
    token_id: Option<u64>,
    principal: Option<String>,
    provider: Option<String>,
}

async fn end_sessions(
    State(state): State<Arc<AppState>>,
    ApiJson(req): ApiJson<EndReq>,
) -> Result<Json<Change>, ApiError> {
    let given = [
        req.token_id.is_some(),
        req.principal.is_some(),
        req.provider.is_some(),
    ];
    if given.iter().filter(|g| **g).count() != 1 {
        return Err(ApiError::Contract(
            "send exactly one of `token_id`, `principal` and `provider`".into(),
        ));
    }
    let sessions_ended = state
        .end_sessions(|e| match req.token_id {
            Some(id) => e.session.token_id() == id,
            None => selects(&req.principal, &req.provider, e),
        })
        .await;
    Ok(Json(Change { sessions_ended }))
}
