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
//! - **`serve.cors_loopback`** admits any page served from a loopback address — `localhost`,
//!   `127.0.0.1` or `[::1]`, on any port — on the **viewer plane only**, as if that origin had
//!   been listed in `serve.cors_origins`. It exists because a notebook front end's origin is a
//!   port the kernel chose, so no operator can enumerate it (`python-sdk.md` §7). It is a
//!   statement about which pages may present a token, on decision 0102's argument, and it is
//!   bounded: a page served from a loopback address is already running on the machine the
//!   deployment runs on. It is off by default and logged once at startup at `info`.
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
    layer(&origins, state.cors_loopback)
}

/// The session plane's layer: the development list, and **only** the development list.
///
/// `/session/authorise` is gated by the deployment's session credential, which a browser must
/// never hold. `serve.cors_origins` names pages that may present a *token*; extending it to this
/// plane would name pages that may present the credential that mints tokens, which is the thing
/// decision 0102 declined. Reading the field here rather than taking it as an argument is what
/// makes that a property of this module instead of a rule every caller has to remember.
pub fn session_layer(state: &AppState) -> Option<CorsLayer> {
    layer(&state.dev_cors_origins, false)
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
fn layer(origins: &[String], loopback: bool) -> Option<CorsLayer> {
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter(|o| o.trim() != "*")
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if parsed.is_empty() && !loopback {
        return None;
    }
    // `serve.cors_loopback` widens the match from equality against a list to equality against a
    // list *or* the loopback rule, and nothing else about the layer moves: the same methods, the
    // same request headers, the same exposed set. A wildcard is as unreachable here as it is
    // above — [`is_loopback_origin`] matches a host exactly, so the predicate refuses every
    // origin the list would have refused but for a loopback one.
    let allow = if loopback {
        AllowOrigin::predicate(move |origin, _| {
            parsed.iter().any(|listed| listed == origin) || is_loopback_origin(origin)
        })
    } else {
        AllowOrigin::list(parsed)
    };
    Some(
        CorsLayer::new()
            .allow_origin(allow)
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

/// Whether an `Origin` header is a page served from a loopback address, under
/// `serve.cors_loopback`.
///
/// The rule is `http` or `https`, a host of exactly `localhost`, `127.0.0.1` or `[::1]`, and any
/// port or none. **The host is matched whole**, so `localhost.evil.example` and
/// `127.0.0.1.evil.example` — names an attacker can register and serve from anywhere — are
/// refused, as is any origin carrying a path, a query or userinfo, which no browser sends and
/// which would otherwise let a suffix ride in ahead of the host. The comparison is byte-exact:
/// browsers serialise an origin lowercased, so an upper-case scheme or host is not a form this
/// header arrives in and is not one this admits.
///
/// Every other address is refused. `0.0.0.0` and the rest of `127.0.0.0/8` are not loopback
/// origins here: what the key states is that a page served from the machine the deployment runs
/// on may present a token, and the three spellings above are what a local server gives a browser.
fn is_loopback_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };
    // The host and the port, split without a URL parser: an `Origin` is a scheme, a host and an
    // optional port and nothing else, so anything a split leaves over — a path, a query, userinfo
    // — is what makes this not an origin, and is refused rather than trimmed away.
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((inside, tail)) = rest.split_once(']') else {
            return false;
        };
        if inside != "::1" {
            return false;
        }
        let port = match tail {
            "" => None,
            tail => match tail.strip_prefix(':') {
                Some(port) => Some(port),
                None => return false,
            },
        };
        (inside, port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    let port_ok = match port {
        None => true,
        Some(port) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
    };
    port_ok && matches!(host, "localhost" | "127.0.0.1" | "::1")
}

#[cfg(test)]
mod tests {
    use super::{is_loopback_origin, layer};
    use axum::http::HeaderValue;

    fn loopback(origin: &str) -> bool {
        is_loopback_origin(&HeaderValue::from_str(origin).unwrap())
    }

    #[test]
    fn an_empty_origin_list_mounts_no_layer() {
        assert!(layer(&[], false).is_none());
    }

    #[test]
    fn an_unparseable_origin_is_dropped_rather_than_panicking() {
        // A newline cannot be a header value. Dropping it leaves nothing, so nothing is mounted —
        // which is the fail-closed direction for a key whose whole purpose is to widen access.
        assert!(layer(&["http://bad\nvalue".to_string()], false).is_none());
    }

    #[test]
    fn a_wildcard_is_dropped_rather_than_panicking() {
        // `AllowOrigin::list` panics on `*`. Parse refuses one before it can reach here; this
        // keeps a state built by hand from taking the process down, and leaves no layer rather
        // than a layer that allows everything.
        assert!(layer(&["*".to_string()], false).is_none());
        assert!(layer(
            &["*".to_string(), "http://localhost:5173".to_string()],
            false
        )
        .is_some());
    }

    #[test]
    fn a_valid_origin_mounts_a_layer() {
        assert!(layer(&["http://localhost:5173".to_string()], false).is_some());
    }

    #[test]
    fn the_three_loopback_spellings_are_admitted_on_any_port() {
        for origin in [
            "http://localhost",
            "http://localhost:5173",
            "https://localhost:8888",
            "http://127.0.0.1:1",
            "http://[::1]",
            "http://[::1]:65535",
        ] {
            assert!(loopback(origin), "{origin} is a loopback origin");
        }
    }

    /// The suffix attack the whole-host match exists to refuse: both of these are names an
    /// attacker registers and serves from an address that is not this machine.
    #[test]
    fn a_host_that_merely_begins_with_a_loopback_name_is_refused() {
        for origin in [
            "http://localhost.evil.example",
            "https://localhost.evil.example:443",
            "http://127.0.0.1.evil.example",
            "http://127.0.0.1.evil.example:8080",
            "http://evil.example",
            "http://notlocalhost",
        ] {
            assert!(!loopback(origin), "{origin} must be refused");
        }
    }

    #[test]
    fn a_non_http_scheme_a_malformed_port_and_an_authority_that_is_not_one_are_refused() {
        for origin in [
            "file://localhost",
            "vscode-webview://localhost",
            "null",
            "*",
            "http://localhost:",
            "http://localhost:80x",
            "http://user@localhost",
            "http://localhost/path",
            "http://127.0.0.2",
            "http://0.0.0.0:5173",
            "http://[::2]",
        ] {
            assert!(!loopback(origin), "{origin} must be refused");
        }
    }

    /// With no list at all, `cors_loopback` still mounts a layer — it is the notebook's whole
    /// case, where no origin can be enumerated in advance.
    #[test]
    fn cors_loopback_mounts_a_layer_with_no_list_and_off_mounts_none() {
        assert!(layer(&[], true).is_some());
        assert!(layer(&[], false).is_none());
    }
}
