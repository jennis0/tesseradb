//! Credentials on the three listeners, and minting sessions from them.
//!
//! A bearer credential is the operator credential, an API key (`tsk_…`) or an OIDC access token.
//! The operator credential authenticates the superuser, which holds every permission and
//! `bypass`. An API key authenticates its local principal, with the permissions the catalogue
//! resolves for that key. An access token authenticates an OIDC identity, whose terms and
//! permissions the catalogue derives from its claims.
//!
//! A session is minted from a resolution the catalogue made at some generation. The engine builds
//! the session's authorised set without any lock held, and the session is registered under the
//! registry's lock only if the catalogue is still at that generation. A change applies its report
//! under the same lock after it commits, so a session is either registered before the change ends
//! it or resolved again after the change.

use std::sync::Arc;

use serde_json::Value;
use sha2::{Digest, Sha256};

use tessera_catalogue::{Catalogue, Permission, PermissionSet, Resolution};

use crate::error::ApiError;
use crate::state::{now_secs, AppState, Principal, SessionEntry};

/// A caller whose credential has been checked, with what it holds.
#[derive(Clone, Debug)]
pub struct Caller {
    pub principal: Principal,
    /// The API key presented, by prefix.
    pub api_key: Option<String>,
    pub resolution: Resolution,
    /// When the credential stops being accepted: the key's expiry or the token's `exp`.
    pub expires_at: Option<u64>,
    /// How the catalogue resolves this caller again.
    source: Source,
}

impl Caller {
    pub fn holds(&self, p: Permission) -> bool {
        self.resolution.permissions.contains(p)
    }

    /// Refuses with `403 forbidden` unless the caller holds `p`.
    pub fn require(&self, p: Permission) -> Result<(), ApiError> {
        if self.holds(p) {
            Ok(())
        } else {
            Err(ApiError::Forbidden(format!(
                "this credential does not hold the `{p}` permission; ask an administrator to grant \
                 it"
            )))
        }
    }

    /// The local principal's name, where the caller is one.
    pub fn local_name(&self) -> Option<&str> {
        match &self.principal {
            Principal::Local(name) => Some(name),
            _ => None,
        }
    }

    fn superuser() -> Caller {
        Caller {
            principal: Principal::Superuser,
            api_key: None,
            resolution: Resolution {
                terms: Default::default(),
                permissions: Permission::ALL.into_iter().collect(),
                bypass: true,
                generation: 0,
            },
            expires_at: None,
            source: Source::Superuser,
        }
    }
}

/// What a caller's resolution is made from, so it can be made again at a later generation.
#[derive(Clone, Debug)]
enum Source {
    Superuser,
    Local {
        principal: String,
        api_key: Option<String>,
    },
    Claims {
        provider: String,
        claims: Value,
    },
}

impl Source {
    fn resolve(&self, catalogue: &Catalogue) -> Option<Resolution> {
        match self {
            Source::Superuser => Some(Caller::superuser().resolution),
            Source::Local { principal, api_key } => catalogue.resolve(principal, api_key.as_deref()),
            Source::Claims { provider, claims } => catalogue.resolve_claims(provider, claims),
        }
    }
}

/// Which credentials a listener accepts as a bearer.
#[derive(Clone, Copy)]
pub struct Accepts {
    pub operator: bool,
    pub api_key: bool,
    pub access_token: bool,
}

/// Compares two secrets in constant time by their SHA-256 digests, so a wrong guess takes the
/// same time wherever it diverges.
pub fn same_secret(presented: &str, expected: &str) -> bool {
    let a: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
    let b: [u8; 32] = Sha256::digest(expected.as_bytes()).into();
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Authenticates a bearer credential of a kind `accepts` names. Anything not accepted is
/// `401 bad-credential`, with one answer whatever the cause.
pub async fn authenticate(
    state: &AppState,
    bearer: Option<&str>,
    accepts: Accepts,
) -> Result<Caller, ApiError> {
    let token = bearer.map(str::trim).ok_or(ApiError::BadCredential)?;
    if accepts.operator && same_secret(token, &state.operator_credential) {
        return Ok(Caller::superuser());
    }
    if token.starts_with("tsk_") {
        return if accepts.api_key {
            api_key(&state.catalogue, token)
        } else {
            Err(ApiError::BadCredential)
        };
    }
    if accepts.access_token {
        return access_token(state, token).await;
    }
    Err(ApiError::BadCredential)
}

/// A local principal by API key.
pub fn api_key(catalogue: &Catalogue, key: &str) -> Result<Caller, ApiError> {
    let auth = catalogue
        .verify_api_key(key)
        .map_err(|_| ApiError::BadCredential)?;
    let prefix = auth.api_key.expect("an API key authenticates with its prefix");
    let expires_at = catalogue
        .api_keys(&auth.principal)
        .into_iter()
        .find(|k| k.prefix == prefix)
        .and_then(|k| k.expires_at);
    local(catalogue, auth.principal, Some(prefix), expires_at)
}

/// A local principal by name and password. The check is argon2id, so it runs off the async
/// threads.
pub async fn password(
    state: &Arc<AppState>,
    principal: String,
    password: String,
) -> Result<Caller, ApiError> {
    state
        .blocking(move |state| {
            let auth = state
                .catalogue
                .verify_password(&principal, &password)
                .map_err(|_| ApiError::BadCredential)?;
            local(&state.catalogue, auth.principal, None, None)
        })
        .await
}

fn local(
    catalogue: &Catalogue,
    principal: String,
    api_key: Option<String>,
    expires_at: Option<u64>,
) -> Result<Caller, ApiError> {
    let source = Source::Local {
        principal: principal.clone(),
        api_key: api_key.clone(),
    };
    let resolution = source.resolve(catalogue).ok_or(ApiError::BadCredential)?;
    Ok(Caller {
        principal: Principal::Local(principal),
        api_key,
        resolution,
        expires_at,
        source,
    })
}

/// An OIDC identity by access token.
pub async fn access_token(state: &AppState, token: &str) -> Result<Caller, ApiError> {
    let accepted = state
        .oidc
        .verify(&state.catalogue, token)
        .await
        .ok_or(ApiError::BadCredential)?;
    let source = Source::Claims {
        provider: accepted.provider.clone(),
        claims: accepted.claims,
    };
    let resolution = source
        .resolve(&state.catalogue)
        .ok_or(ApiError::BadCredential)?;
    Ok(Caller {
        principal: Principal::Oidc {
            provider: accepted.provider,
            issuer: accepted.issuer,
            subject: accepted.subject,
        },
        api_key: None,
        resolution,
        expires_at: Some(accepted.expires_at),
        source,
    })
}

/// A local principal named by an `authorise-as` caller. An unknown or disabled principal is
/// `404 unknown`: the caller may act as any principal, so the answer tells it nothing it could
/// not learn by acting.
pub fn named(catalogue: &Catalogue, principal: &str) -> Result<Caller, ApiError> {
    let principal = principal.trim().to_owned();
    local(catalogue, principal.clone(), None, None).map_err(|_| {
        ApiError::Unknown(format!(
            "there is no enabled principal named `{principal}`; list the principals to find the \
             name"
        ))
    })
}

/// A minted session, as the session plane and the login route answer it.
pub struct Minted {
    pub token: String,
    pub token_id: u64,
    pub expires_at: u64,
}

/// How many times a session is resolved again when the catalogue changes while it is minted.
const MINT_ATTEMPTS: usize = 3;

/// The permissions a session carries: `read` and `write` only. `admin`, `authorise-as` and
/// `bypass` are never carried by a session.
fn session_permissions(p: PermissionSet) -> PermissionSet {
    [Permission::Read, Permission::Write]
        .into_iter()
        .filter(|q| p.contains(*q))
        .collect()
}

/// Mints a session for `target`, or for `target` through `minter`'s `authorise-as`. The target
/// must hold `read`. Each attempt resolves both again, so a change that commits while the
/// engine builds the authorised set is either applied to the registered session or seen by the
/// next attempt.
pub async fn mint(
    state: &Arc<AppState>,
    target: Caller,
    minter: Option<Caller>,
) -> Result<Minted, ApiError> {
    for _ in 0..MINT_ATTEMPTS {
        let Some(resolution) = target.source.resolve(&state.catalogue) else {
            return Err(ApiError::BadCredential);
        };
        if let Some(minter) = &minter {
            let now = minter.source.resolve(&state.catalogue);
            match now {
                Some(r) if r.generation == resolution.generation => {
                    if !r.permissions.contains(Permission::AuthoriseAs) {
                        return Err(ApiError::Forbidden(
                            "this API key does not hold the `authorise-as` permission; ask an \
                             administrator to grant it"
                                .into(),
                        ));
                    }
                }
                Some(_) => continue,
                None => return Err(ApiError::BadCredential),
            }
        }
        if !resolution.permissions.contains(Permission::Read) {
            return Err(ApiError::Forbidden(
                "the principal does not hold the `read` permission, which a session needs; ask \
                 an administrator to grant it"
                    .into(),
            ));
        }
        let auth_data = serde_json::to_vec(&serde_json::json!({ "terms": resolution.terms }))
            .expect("a list of strings serialises");
        let session = state
            .gated(move |state| {
                state
                    .engine
                    .authorise(&auth_data)
                    .map_err(crate::error::map_engine_error)
            })
            .await?;
        let now = now_secs();
        let expires_at = [
            Some(session.expires_at()),
            target.expires_at,
            minter.as_ref().and_then(|m| m.expires_at),
        ]
        .into_iter()
        .flatten()
        .min()
        .expect("the engine's deadline is always present");
        let minted = Minted {
            token: session.token().to_owned(),
            token_id: session.token_id(),
            expires_at,
        };
        let entry = SessionEntry {
            session: Arc::new(session),
            principal: target.principal.clone(),
            api_key: minter
                .as_ref()
                .map_or(target.api_key.clone(), |m| m.api_key.clone()),
            minted_by: minter.as_ref().and_then(|m| m.local_name().map(str::to_owned)),
            permissions: session_permissions(resolution.permissions),
            created_at: now,
            expires_at,
        };
        let swept = {
            let mut sessions = state.sessions.lock();
            if state.catalogue.generation() != resolution.generation {
                None
            } else {
                Some(sessions.insert(entry, now))
            }
        };
        match swept {
            Some(swept) => {
                state.prune_sessions(swept);
                return Ok(minted);
            }
            None => state.prune_sessions(vec![minted.token_id]),
        }
    }
    Err(ApiError::Backpressure {
        retry_after_s: crate::error::RETRY_AFTER_SECS,
        cause: crate::error::ShedCause::CatalogueChanging,
    })
}
