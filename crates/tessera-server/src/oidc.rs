//! OIDC access tokens. A token is accepted when its signature verifies against a key its
//! provider publishes at the provider's JWKS URL, its `iss` is the provider's issuer, its `aud`
//! holds the provider's audience, `exp` has not passed and `nbf`, where present, has. The claims
//! of an accepted token go to the catalogue, which maps them to terms and groups.
//!
//! The providers tried are those whose issuer is the token's unverified `iss`, and only their keys
//! can verify it, so a token cannot borrow another provider's claim rules. Only asymmetric algorithms
//! are accepted: a provider publishes public keys, and a shared-secret algorithm would let anyone
//! holding the published key sign.
//!
//! A token whose `typ` header is present must name a JWT or an access token (`at+jwt`, RFC 9068).
//! Any other type, such as `logout+jwt` or `id_token+jwt`, is refused. An OpenID Connect ID token
//! usually carries `typ: JWT` or no `typ`, so this rule cannot tell it apart; its `aud` is the
//! client's id, so a provider whose `audience` names the API, and not a client, refuses it.
//!
//! When more than one provider with the token's issuer accepts it, for example because its `aud`
//! names both providers' audiences, the token is refused: nothing in it says which provider's
//! claim rules apply.
//!
//! Keys are fetched when first needed and kept for [`KEYS_MAX_AGE`]. A token naming a key the
//! cached set lacks fetches the set again. Each URL has one fetch in flight at a time, and is
//! fetched at most once per refetch interval, [`REFETCH_INTERVAL`], whether the last fetch
//! succeeded or failed. A fetch runs as its own task, so it completes and stores what it found
//! when the request that started it goes away. A request that has fresh keys reads them without
//! waiting for a fetch in flight. A provider's key rotation is therefore picked up, a stream of
//! unknown key ids costs one fetch per interval, and a provider that is down is not asked again
//! by every request. A refetch that fails leaves the keys of the last successful fetch in use
//! until they are [`KEYS_CEILING`] old. After that, the provider's tokens are refused until a
//! fetch succeeds. [`Verifier::with_intervals`] sets all three durations.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use parking_lot::Mutex;
use serde_json::Value;

use tessera_catalogue::{Catalogue, Provider};

const KEYS_MAX_AGE: Duration = Duration::from_secs(3600);
const KEYS_CEILING: Duration = Duration::from_secs(24 * 3600);
const REFETCH_INTERVAL: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The largest JWKS document read.
const MAX_JWKS_BYTES: usize = 1 << 20;

const ASYMMETRIC: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

/// An access token that verified.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub provider: String,
    pub issuer: String,
    pub subject: String,
    /// Seconds since the Unix epoch.
    pub expires_at: u64,
    pub claims: Value,
}

/// One URL's keys. The lock is never held across an await.
#[derive(Default)]
struct Keys {
    /// The keys of the last successful fetch, and when it ended.
    keys: Option<(Arc<Vec<Jwk>>, Instant)>,
    /// When the last fetch, successful or not, began.
    attempted: Option<Instant>,
    /// Why the last fetch failed, until one succeeds.
    failure: Option<String>,
    /// Changes when the fetch in flight ends.
    in_flight: Option<tokio::sync::watch::Receiver<()>>,
}

/// Verifies access tokens and caches each provider's published keys by URL.
pub struct Verifier {
    http: reqwest::Client,
    keys: Mutex<HashMap<String, Arc<Mutex<Keys>>>>,
    refetch_interval: Duration,
    keys_max_age: Duration,
    keys_ceiling: Duration,
}

impl Default for Verifier {
    fn default() -> Self {
        Verifier::new()
    }
}

impl Verifier {
    pub fn new() -> Verifier {
        Verifier::with_intervals(REFETCH_INTERVAL, KEYS_MAX_AGE, KEYS_CEILING)
    }

    /// A verifier that fetches each URL at most once per `refetch_interval`, fetches keys older
    /// than `keys_max_age` again, and uses no keys older than `keys_ceiling`.
    pub fn with_intervals(
        refetch_interval: Duration,
        keys_max_age: Duration,
        keys_ceiling: Duration,
    ) -> Verifier {
        Verifier {
            http: reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client with a timeout and no redirects builds"),
            keys: Mutex::new(HashMap::new()),
            refetch_interval,
            keys_max_age,
            keys_ceiling,
        }
    }

    /// The token's provider, identity, expiry and claims, or `None` when it is not accepted. The
    /// reason is logged and not returned, so a caller learns nothing about which check failed.
    pub async fn verify(&self, catalogue: &Catalogue, token: &str) -> Option<Accepted> {
        match self.check(catalogue, token).await {
            Ok(accepted) => Some(accepted),
            Err(why) => {
                tracing::info!(reason = %why, "an OIDC access token was not accepted");
                None
            }
        }
    }

    async fn check(&self, catalogue: &Catalogue, token: &str) -> Result<Accepted, String> {
        let header = jsonwebtoken::decode_header(token).map_err(|e| format!("header: {e}"))?;
        if !ASYMMETRIC.contains(&header.alg) {
            return Err(format!("algorithm {:?} is not accepted", header.alg));
        }
        if let Some(typ) = &header.typ {
            let typ = typ.to_ascii_lowercase();
            if !matches!(typ.as_str(), "jwt" | "at+jwt" | "application/at+jwt") {
                return Err(format!("a token of type `{typ}` is not an access token"));
            }
        }
        let issuer = unverified_issuer(token)?;
        let providers: Vec<Provider> = catalogue
            .providers()
            .into_iter()
            .map(|p| p.provider)
            .filter(|p| p.issuer == issuer)
            .collect();
        if providers.is_empty() {
            return Err(format!("no provider has issuer `{issuer}`"));
        }
        let mut why = String::new();
        let mut accepted = Vec::new();
        for provider in providers {
            match self.check_with(provider, &header, token).await {
                Ok(a) => accepted.push(a),
                Err(e) => why = e,
            }
        }
        match accepted.len() {
            0 => Err(why),
            1 => Ok(accepted.pop().expect("one was accepted")),
            _ => {
                let names: Vec<&str> = accepted.iter().map(|a| a.provider.as_str()).collect();
                Err(format!("providers {names:?} each accept the token"))
            }
        }
    }

    async fn check_with(
        &self,
        provider: Provider,
        header: &jsonwebtoken::Header,
        token: &str,
    ) -> Result<Accepted, String> {
        let mut validation = Validation::new(header.alg);
        validation.leeway = 0;
        validation.validate_nbf = true;
        validation.set_issuer(&[&provider.issuer]);
        validation.set_audience(&[&provider.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);

        let mut refetched = false;
        loop {
            let keys = self.keys_for(&provider.jwks_url, refetched).await?;
            let candidates: Vec<&Jwk> = keys
                .iter()
                .filter(|k| match (&header.kid, &k.common.key_id) {
                    (Some(want), Some(have)) => want == have,
                    (Some(_), None) => false,
                    (None, _) => true,
                })
                .filter(|k| {
                    k.common
                        .key_algorithm
                        .is_none_or(|a| format!("{a:?}") == format!("{:?}", header.alg))
                })
                .collect();
            for key in &candidates {
                let Ok(decoding) = DecodingKey::from_jwk(key) else {
                    continue;
                };
                if let Ok(data) = jsonwebtoken::decode::<Value>(token, &decoding, &validation) {
                    let claims = data.claims;
                    let subject = claims["sub"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or("`sub` is not a non-empty string")?
                        .to_owned();
                    let expires_at = claims["exp"].as_u64().ok_or("`exp` is not a whole number")?;
                    return Ok(Accepted {
                        issuer: provider.issuer.clone(),
                        provider: provider.name,
                        subject,
                        expires_at,
                        claims,
                    });
                }
            }
            if refetched || (header.kid.is_some() && !candidates.is_empty()) {
                return Err("no published key verifies the token".into());
            }
            refetched = true;
        }
    }

    /// The keys published at `url`. They are fetched when there are none, when they are older
    /// than the maximum age, or when `refetch` is set, unless a fetch began within the refetch
    /// interval. A caller waits for a fetch in flight unless it holds fresh keys and asks for no
    /// refetch. The answer is the keys held once any fetch has ended, unless they are older than
    /// the ceiling, or the failure when there are none.
    async fn keys_for(&self, url: &str, refetch: bool) -> Result<Arc<Vec<Jwk>>, String> {
        let slot = Arc::clone(self.keys.lock().entry(url.to_owned()).or_default());
        let wait = {
            let mut held = slot.lock();
            let fresh = held
                .keys
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < self.keys_max_age);
            let tried_lately = held
                .attempted
                .is_some_and(|at| at.elapsed() < self.refetch_interval);
            if fresh && !refetch {
                None
            } else if let Some(wait) = held.in_flight.as_ref().filter(|w| w.has_changed().is_ok()) {
                Some(wait.clone())
            } else if tried_lately {
                None
            } else {
                Some(self.start_fetch(url, &slot, &mut held))
            }
        };
        let waited = wait.is_some();
        if let Some(mut wait) = wait {
            // The sender is dropped when the fetch ends, which ends the wait either way.
            let _ = wait.changed().await;
        }
        let held = slot.lock();
        match &held.keys {
            Some((keys, at)) if at.elapsed() < self.keys_ceiling => {
                if let Some(why) = held.failure.as_ref().filter(|_| waited) {
                    tracing::warn!(reason = %why, "a JWKS refetch failed; the keys held are kept");
                }
                Ok(Arc::clone(keys))
            }
            Some(_) => Err(format!(
                "the keys held for {url} are older than {:?} and a refetch failed",
                self.keys_ceiling
            )),
            None => Err(held
                .failure
                .clone()
                .unwrap_or_else(|| format!("fetching {url} failed lately"))),
        }
    }

    /// Starts a fetch of `url` as its own task, which stores what it finds in `slot`.
    fn start_fetch(
        &self,
        url: &str,
        slot: &Arc<Mutex<Keys>>,
        held: &mut Keys,
    ) -> tokio::sync::watch::Receiver<()> {
        let (done, wait) = tokio::sync::watch::channel(());
        held.attempted = Some(Instant::now());
        held.in_flight = Some(wait.clone());
        let (http, url, slot) = (self.http.clone(), url.to_owned(), Arc::clone(slot));
        tokio::spawn(async move {
            let fetched = fetch(&http, &url).await;
            let mut held = slot.lock();
            match fetched {
                Ok(keys) => {
                    held.keys = Some((Arc::new(keys), Instant::now()));
                    held.failure = None;
                }
                Err(why) => held.failure = Some(why),
            }
            held.in_flight = None;
            drop(done);
        });
        wait
    }
}

/// The keys of the JWKS document at `url`. A key this crate cannot read is skipped.
async fn fetch(http: &reqwest::Client, url: &str) -> Result<Vec<Jwk>, String> {
    let mut resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("fetching {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("fetching {url}: {}", resp.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("reading {url}: {e}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_JWKS_BYTES {
            return Err(format!("{url} is larger than {MAX_JWKS_BYTES} bytes"));
        }
    }
    let doc: Value =
        serde_json::from_slice(&body).map_err(|e| format!("{url} is not JSON: {e}"))?;
    let keys = doc["keys"]
        .as_array()
        .ok_or_else(|| format!("{url} has no `keys` array"))?;
    Ok(keys
        .iter()
        .filter_map(|k| serde_json::from_value::<Jwk>(k.clone()).ok())
        .collect())
}

/// The `iss` claim of a token whose signature has not been checked, used only to choose the
/// provider whose keys check it.
fn unverified_issuer(token: &str) -> Result<String, String> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or("the token is not three dot-separated parts")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| format!("payload: {e}"))?;
    let claims: Value = serde_json::from_slice(&bytes).map_err(|e| format!("payload: {e}"))?;
    claims["iss"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "`iss` is not a string".into())
}
