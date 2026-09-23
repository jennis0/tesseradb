//! CORS for the viewer and session planes. The control plane is never wrapped.
//!
//! - `serve.dev_cors_origins` is for development and covers both planes, so a page on a laptop
//!   can authorise and then use its token. It is off unless set, and warned about at startup.
//! - `serve.cors_origins` names the pages that may present tokens, on the viewer plane only.
//! - `serve.cors_loopback` admits any page served from a loopback address on the viewer plane,
//!   for notebook front ends whose port no operator can know in advance.
//!
//! An origin list is not an authorisation boundary: it decides which page a browser hands a
//! response to, and the bearer token decides what the response contains.

use axum::http::{HeaderName, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::state::AppState;

/// Response headers a browser page may read; `fetch` hides any not listed. They carry timings,
/// the coordinates a client revalidates a cached response by, and whether a region count is exact.
const EXPOSED: [&str; 7] = [
    "etag",
    "x-tessera-identity-key",
    "x-tessera-stale",
    "x-tessera-pin",
    "x-tessera-server-us",
    "x-tessera-admission-us",
    "x-tessera-region",
];

/// The viewer plane's layer, from the development and production lists together.
pub fn viewer_layer(state: &AppState) -> Option<CorsLayer> {
    let origins: Vec<String> = state
        .limits
        .dev_cors_origins
        .iter()
        .chain(state.limits.cors_origins.iter())
        .cloned()
        .collect();
    layer(&origins, state.limits.cors_loopback)
}

/// The session plane's layer, from the development list only. The production list never reaches
/// the session plane, because the session credential must never be held by a browser; reading
/// the field here means no caller can pass the other list.
pub fn session_layer(state: &AppState) -> Option<CorsLayer> {
    layer(&state.limits.dev_cors_origins, false)
}

/// The layer for an origin list, or `None` (no layer mounted) when nothing valid is configured.
/// Unparseable origins and `*` are dropped, since [`AllowOrigin::list`] panics on a wildcard.
fn layer(origins: &[String], loopback: bool) -> Option<CorsLayer> {
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter(|o| o.trim() != "*")
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if parsed.is_empty() && !loopback {
        return None;
    }
    // Loopback widens only which origins match; the methods and headers are unchanged.
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
            // No `allow_credentials`: the token is never a cookie, and credentials mode would make
            // the browser attach ambient credentials to every call.
            .allow_headers([
                HeaderName::from_static("authorization"),
                HeaderName::from_static("content-type"),
            ])
            .expose_headers(EXPOSED.map(HeaderName::from_static)),
    )
}

/// Whether an `Origin` is `http` or `https` on exactly `localhost`, `127.0.0.1` or `[::1]`, with
/// any port. The host is matched whole and byte-exact, so `localhost.evil.example`, an upper-case
/// host and anything with a path, query or userinfo are refused.
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
    // Anything left after the host and port, such as a path or userinfo, refuses the origin.
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
        // A newline cannot be a header value, so nothing is mounted.
        assert!(layer(&["http://bad\nvalue".to_string()], false).is_none());
    }

    #[test]
    fn a_wildcard_is_dropped_rather_than_panicking() {
        // Parse refuses `*`; this covers a state built by hand.
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
            // Authorities that are not origins are refused, not trimmed into one.
            "http://localhost@evil",
            "http://[::1]:x",
            "http://LOCALHOST",
            "http://127.0.0.1.",
            "http://localhost:5173:1",
            "http://[::1]x",
        ] {
            assert!(!loopback(origin), "{origin} must be refused");
        }
    }

    /// With no list at all, `cors_loopback` still mounts a layer. That is the notebook's whole
    /// case, where no origin can be enumerated in advance.
    #[test]
    fn cors_loopback_mounts_a_layer_with_no_list_and_off_mounts_none() {
        assert!(layer(&[], true).is_some());
        assert!(layer(&[], false).is_none());
    }
}
