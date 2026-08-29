//! The two browser seams, and the line between them.
//!
//! - **`serve.dev_cors_origins`** is a development affordance and it is off unless typed. It
//!   covers the viewer *and* session planes, so a page on a laptop can call `/session/authorise`
//!   with the deployment's session credential and then use the token it gets back. That is why it
//!   has no default, no environment variable, no wildcard, and a startup warning when it is on.
//! - **`serve.cors_origins`** is a deployment's production statement about which pages may present
//!   its **tokens** (docs/decisions/0102-the-viewer-plane-gains-an-enumerated-cors-origin-list.md).
//!   It covers the **viewer plane only** and is silent at startup. A token is already
//!   per-principal, already scoped by what the server decided that principal may see, and already
//!   expires, so a named origin presenting one creates no authority that did not exist; the
//!   session credential is a different thing entirely and no browser may hold it.
//!
//! The exclusion is structural rather than remembered: [`session_layer`] takes the whole
//! [`AppState`] and reads `dev_cors_origins` itself, so there is no call site that *could* hand
//! the session router the production list. `tests/cors.rs` asserts it from the outside as well.
//!
//! An origin list is not an authorisation boundary and nothing in the engine may come to treat it
//! as one (decision 0102). It decides which page a browser will hand a response to; the bearer
//! decides what the response contains.
//!
//! Scope: the viewer and session planes only. The control plane is never wrapped — it carries the
//! operator credential and no browser has business reaching it. `tests/cors.rs` asserts that
//! absence rather than trusting this sentence.

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::AppState;

/// Response headers a browser client is allowed to read.
///
/// Without these, `fetch` hides them from the page and the viewer's stats panel silently shows
/// nothing — a failure that reads as "the server emits no timings" rather than as a CORS
/// configuration hiding them. `x-tessera-stage-ns` is retired (contracts §3.2 r26 — the stage
/// breakdown rides the trailer frame, in-body and outside CORS's reach). `etag`,
/// `x-tessera-identity-key` and `x-tessera-stale` are the delta-serving coordinates
/// (`delta-serving.md` §2): a browser client that cannot key a replica cannot revalidate one, and
/// `etag` is not on the CORS safelist despite being a standard header. `x-tessera-region` is the
/// region leaf's exactness verdict (`selection-operand.md` §6): a browser client that cannot read
/// it renders every region count inexact, which is the fail-closed reading and not the answer.
const EXPOSED: [&str; 7] = [
    "etag",
    "x-tessera-identity-key",
    "x-tessera-stale",
    "x-tessera-pin",
    "x-tessera-server-us",
    "x-tessera-admission-us",
    "x-tessera-region",
];

/// The viewer plane's layer: the development list **and** the production list, together.
///
/// Both may be set — a laptop pointed at a deployment that also serves a drop-in — and a duplicate
/// origin across the two is not an error, only a repeated entry in a list that is matched by
/// equality.
pub fn viewer_layer(state: &AppState) -> Option<CorsLayer> {
    let origins: Vec<String> = state
        .dev_cors_origins
        .iter()
        .chain(state.cors_origins.iter())
        .cloned()
        .collect();
    layer(&origins)
}

/// The session plane's layer: the development list, and **only** the development list.
///
/// `/session/authorise` is gated by the deployment's session credential, which a browser must
/// never hold. `serve.cors_origins` names pages that may present a *token*; extending it to this
/// plane would name pages that may present the credential that mints tokens, which is the thing
/// decision 0102 declined. Reading the field here rather than taking it as an argument is what
/// makes that a property of this module instead of a rule every caller has to remember.
pub fn session_layer(state: &AppState) -> Option<CorsLayer> {
    layer(&state.dev_cors_origins)
}

/// The layer for a configured origin list, or `None` when there is nothing to configure.
///
/// `None` is the default path and means **no layer is mounted at all** — not a layer that allows
/// nothing. The distinction matters for review: a caller can see from the router that the seam is
/// structurally absent rather than present and configured empty.
///
/// An origin that is not a valid header value is dropped rather than panicking the process. If
/// that leaves the list empty the result is `None`, so an operator who typed something
/// unparseable gets no CORS and no claim of CORS. A wildcard is dropped on the same footing:
/// [`AllowOrigin::list`] panics on one, and `config::load` refuses one outright
/// ([`crate::config::ConfigError::CorsWildcard`]), so this is the belt for a state assembled
/// without going through parse — the tests' path.
fn layer(origins: &[String]) -> Option<CorsLayer> {
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter(|o| o.trim() != "*")
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if parsed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(parsed))
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            // `Authorization` is the only header that has to be named: the token rides it. There
            // is deliberately no `allow_credentials` — the token is never a cookie, and enabling
            // credentials mode would make the browser attach ambient ones to every viewer call.
            .allow_headers([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
            ])
            .expose_headers(EXPOSED.map(HeaderName::from_static)),
    )
}

#[cfg(test)]
mod tests {
    use super::layer;

    #[test]
    fn an_empty_origin_list_mounts_no_layer() {
        assert!(layer(&[]).is_none());
    }

    #[test]
    fn an_unparseable_origin_is_dropped_rather_than_panicking() {
        // A newline cannot be a header value. Dropping it leaves nothing, so nothing is mounted —
        // which is the fail-closed direction for a key whose whole purpose is to widen access.
        assert!(layer(&["http://bad\nvalue".to_string()]).is_none());
    }

    #[test]
    fn a_wildcard_is_dropped_rather_than_panicking() {
        // `AllowOrigin::list` panics on `*`. Parse refuses one before it can reach here; this
        // keeps a state built by hand from taking the process down, and leaves no layer rather
        // than a layer that allows everything.
        assert!(layer(&["*".to_string()]).is_none());
        assert!(layer(&["*".to_string(), "http://localhost:5173".to_string()]).is_some());
    }

    #[test]
    fn a_valid_origin_mounts_a_layer() {
        assert!(layer(&["http://localhost:5173".to_string()]).is_some());
    }
}
