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
//! Keys are fetched when first needed and kept for [`KEYS_MAX_AGE`]. A token naming a key the
//! cached set lacks fetches the set again, at most once per [`REFETCH_INTERVAL`] for each URL, so
//! a provider's key rotation is picked up and a stream of unknown key ids costs one fetch.

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

struct Keys {
    keys: Arc<Vec<Jwk>>,
    fetched: Instant,
}

/// Verifies access tokens and caches each provider's published keys by URL.
pub struct Verifier {
    http: reqwest::Client,
    keys: Mutex<HashMap<String, Keys>>,
}

impl Default for Verifier {
    fn default() -> Self {
        Verifier::new()
    }
}

impl Verifier {
    pub fn new() -> Verifier {
        Verifier {
            http: reqwest::Client::builder()
                .timeout(FETCH_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client with a timeout and no redirects builds"),
            keys: Mutex::new(HashMap::new()),
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
        for provider in providers {
            match self.check_with(provider, &header, token).await {
                Ok(accepted) => return Ok(accepted),
                Err(e) => why = e,
            }
        }
        Err(why)
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

    /// The keys published at `url`, fetched when absent or stale, or when `refetch` is set and
    /// the last fetch is older than [`REFETCH_INTERVAL`].
    async fn keys_for(&self, url: &str, refetch: bool) -> Result<Arc<Vec<Jwk>>, String> {
        {
            let cached = self.keys.lock();
            if let Some(k) = cached.get(url) {
                let age = k.fetched.elapsed();
                if age < KEYS_MAX_AGE && (!refetch || age < REFETCH_INTERVAL) {
                    return Ok(Arc::clone(&k.keys));
                }
            }
        }
        let keys = Arc::new(self.fetch(url).await?);
        self.keys.lock().insert(
            url.to_owned(),
            Keys {
                keys: Arc::clone(&keys),
                fetched: Instant::now(),
            },
        );
        Ok(keys)
    }

    /// The keys of the JWKS document at `url`. A key this crate cannot read is skipped.
    async fn fetch(&self, url: &str) -> Result<Vec<Jwk>, String> {
        let mut resp = self
            .http
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
