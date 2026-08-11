//! `serve.dev_cors_origins` — a browser seam for the MVP viewer, and nothing else.
//!
//! **This is a development affordance and it is off unless typed.** The viewer plane's bearer is a
//! session token; the session plane's is the operator-configured session credential. Enabling this
//! lets a page served from another origin present either, which is why there is no default, no
//! environment variable, no wildcard, and a startup warning when it is on.
//!
//! The documented integration topology remains T2 with verified assertions (client-interaction
//! §7): credential construction at the integrator's app server, where the authority is. Nothing
//! here revises that, and nothing here should be read as recommending browser-direct
//! authorisation outside a laptop.
//!
//! Scope: the viewer and session planes only. The control plane is never wrapped — it carries the
//! operator credential and no browser has business reaching it. `tests/cors.rs` asserts that
//! absence rather than trusting this sentence.

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// Response headers a browser client is allowed to read.
///
/// Without these, `fetch` hides them from the page and the viewer's stats panel silently shows
/// nothing — a failure that reads as "the server emits no timings" rather than as a CORS
/// configuration hiding them. `x-tessera-stage-ns` is retired (contracts §3.2 r26 — the stage
/// breakdown rides the trailer frame, in-body and outside CORS's reach). `etag`,
/// `x-tessera-identity-key` and `x-tessera-stale` are the delta-serving coordinates
/// (`delta-serving.md` §2): a browser client that cannot read them cannot key a replica at all,
/// and `etag` is not on the CORS safelist despite being a standard header.
const EXPOSED: [&str; 6] = [
    "etag",
    "x-tessera-identity-key",
    "x-tessera-stale",
    "x-tessera-pin",
    "x-tessera-server-us",
    "x-tessera-admission-us",
];

/// The layer for a configured origin list, or `None` when there is nothing to configure.
///
/// `None` is the default path and means **no layer is mounted at all** — not a layer that allows
/// nothing. The distinction matters for review: a caller can see from the router that the seam is
/// structurally absent rather than present and configured empty.
///
/// An origin that is not a valid header value is dropped rather than panicking the process. If
/// that leaves the list empty the result is `None`, so an operator who typed something
/// unparseable gets no CORS and no claim of CORS.
pub fn dev_layer(origins: &[String]) -> Option<CorsLayer> {
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if parsed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(parsed))
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
            ])
            .expose_headers(EXPOSED.map(HeaderName::from_static)),
    )
}

#[cfg(test)]
mod tests {
    use super::dev_layer;

    #[test]
    fn an_empty_origin_list_mounts_no_layer() {
        assert!(dev_layer(&[]).is_none());
    }

    #[test]
    fn an_unparseable_origin_is_dropped_rather_than_panicking() {
        // A newline cannot be a header value. Dropping it leaves nothing, so nothing is mounted —
        // which is the fail-closed direction for a key whose whole purpose is to widen access.
        assert!(dev_layer(&["http://bad\nvalue".to_string()]).is_none());
    }

    #[test]
    fn a_valid_origin_mounts_a_layer() {
        assert!(dev_layer(&["http://localhost:5173".to_string()]).is_some());
    }
}
