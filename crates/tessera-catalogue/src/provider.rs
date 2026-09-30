//! OIDC providers as stored data: the claim rules that turn a token's claims into terms, and the
//! role mappings that give an identity holding an exact term the grants of a local group.
//! Validating a token's signature, issuer, audience and lifetime happens elsewhere; this module
//! sees claims that have already been accepted.
//!
//! A claim path is a dot-separated list of object keys. A key followed by `[*]` takes every
//! element of the array found there. A key that itself holds a dot or a bracket is written in
//! double quotes, with `\"` and `\\` as escapes: `"https://example.org/roles"[*]`.
//!
//! A template produces a term from each value the path reaches. It holds `{value}` at most once,
//! replaced by the value, and may hold literal text beside it, as `group:{value}` does. A template
//! without `{value}` produces itself whenever the path reaches any value. Values are strings,
//! numbers and booleans; the path reaching an object, an array without `[*]`, or null produces
//! nothing.

use std::collections::BTreeSet;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde_json::Value;
use url::{Host, Url};

use crate::names;
use crate::Error;

const PLACEHOLDER: &str = "{value}";

/// An OIDC provider: where its tokens come from, who they are for, where its signing keys are
/// published, how its claims map to terms, and which terms carry a local group's grants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub name: String,
    pub issuer: String,
    pub audience: String,
    pub jwks_url: String,
    pub rules: Vec<ClaimRule>,
    pub role_mappings: Vec<RoleMapping>,
}

/// Reads the claim at `claim` and produces a term from each value there by `template`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRule {
    pub claim: String,
    pub template: String,
}

/// An identity whose claim rules produce exactly `term` receives the terms and permissions
/// granted to the local group `group`. It never receives `bypass`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoleMapping {
    pub term: String,
    pub group: String,
}

/// What a provider produces from one set of claims, before local groups are expanded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaimMapping {
    pub terms: BTreeSet<String>,
    /// Local groups whose terms and permissions are added.
    pub groups: BTreeSet<String>,
}

impl Provider {
    /// The provider with every field trimmed, or the reason it cannot be stored. An `http://`
    /// JWKS URL to a host other than a loopback address is accepted only with
    /// `allow_insecure_jwks`.
    pub(crate) fn validated(&self, allow_insecure_jwks: bool) -> Result<Provider, Error> {
        let name = names::name("provider", &self.name)?;
        let field = |label: &str, raw: &str| -> Result<String, Error> {
            let v = raw.trim();
            if v.is_empty() {
                return Err(Error::Invalid(format!(
                    "provider `{name}` has an empty {label}; write the provider's {label}"
                )));
            }
            Ok(v.to_owned())
        };
        let issuer = field("issuer", &self.issuer)?;
        let audience = field("audience", &self.audience)?;
        let jwks_url = field("JWKS URL", &self.jwks_url)?;
        check_jwks_url(&name, &jwks_url, allow_insecure_jwks)?;
        let rules = self
            .rules
            .iter()
            .map(ClaimRule::validated)
            .collect::<Result<Vec<_>, _>>()?;
        let role_mappings = self
            .role_mappings
            .iter()
            .map(RoleMapping::validated)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Provider {
            name,
            issuer,
            audience,
            jwks_url,
            rules,
            role_mappings,
        })
    }

    /// Applies every claim rule to `claims`, a JSON object, and then every role mapping to the
    /// terms produced. A produced term that a grant could not hold is dropped. That includes
    /// `public` in any case, which every session holds anyway.
    pub fn apply(&self, claims: &Value) -> ClaimMapping {
        let mut terms = BTreeSet::new();
        for rule in &self.rules {
            rule.produce(claims, &mut terms);
        }
        let groups = self
            .role_mappings
            .iter()
            .filter(|m| terms.contains(&m.term))
            .map(|m| m.group.clone())
            .collect();
        ClaimMapping { terms, groups }
    }

    /// Whether a role mapping of this provider names the local group `group`.
    pub(crate) fn maps_to(&self, group: &str) -> bool {
        self.role_mappings.iter().any(|m| m.group == group)
    }
}

impl ClaimRule {
    fn validated(&self) -> Result<ClaimRule, Error> {
        let claim = self.claim.trim().to_owned();
        parse_path(&claim)?;
        let template = self.template.trim();
        if template.matches(PLACEHOLDER).count() > 1 {
            return Err(Error::Invalid(format!(
                "the template `{template}` holds {{value}} more than once; write it at most once"
            )));
        }
        let template = names::term(template)?;
        Ok(ClaimRule { claim, template })
    }

    fn produce(&self, claims: &Value, out: &mut BTreeSet<String>) {
        // A stored rule's path has been parsed once already; one that fails here was not
        // validated and produces nothing.
        let Ok(path) = parse_path(&self.claim) else {
            return;
        };
        let mut values = Vec::new();
        walk(claims, &path, &mut values);
        for value in values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
            if let Ok(term) = names::term(&self.template.replacen(PLACEHOLDER, value, 1)) {
                out.insert(term);
            }
        }
    }
}

impl RoleMapping {
    fn validated(&self) -> Result<RoleMapping, Error> {
        Ok(RoleMapping {
            term: names::term(&self.term)?,
            group: names::name("group", &self.group)?,
        })
    }
}

/// Refuses a JWKS URL other than `https://`, or `http://` to `localhost`, `127.0.0.1` or
/// `[::1]`, unless `allow_insecure` is set. A URL that does not parse is refused either way.
fn check_jwks_url(provider: &str, url: &str, allow_insecure: bool) -> Result<(), Error> {
    let bad = |why: &str| {
        Error::Invalid(format!(
            "provider `{provider}` has JWKS URL `{url}`, {why}; write an https:// URL, or an \
             http:// URL to localhost, 127.0.0.1 or [::1]"
        ))
    };
    let parsed = Url::parse(url).map_err(|e| bad(&format!("which is not a URL ({e})")))?;
    let secure = match parsed.scheme() {
        "https" => true,
        "http" => false,
        _ => return Err(bad("which is not an http:// or https:// URL")),
    };
    let Some(host) = parsed.host() else {
        return Err(bad("which names no host"));
    };
    if secure || allow_insecure || is_loopback(&host) {
        return Ok(());
    }
    Err(Error::InsecureJwks {
        provider: provider.to_owned(),
        url: url.to_owned(),
    })
}

fn is_loopback(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(a) => *a == Ipv4Addr::LOCALHOST,
        Host::Ipv6(a) => *a == Ipv6Addr::LOCALHOST,
    }
}

struct Segment {
    key: String,
    each: bool,
}

fn parse_path(raw: &str) -> Result<Vec<Segment>, Error> {
    let bad = |why: &str| {
        Error::Invalid(format!(
            "the claim path `{raw}` {why}; write keys separated by dots, each optionally \
             followed by [*], quoting a key that holds a dot or a bracket"
        ))
    };
    let mut segments = Vec::new();
    let mut chars = raw.chars().peekable();
    loop {
        let mut key = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    None => return Err(bad("has an unclosed quote")),
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some(c @ ('"' | '\\')) => key.push(c),
                        _ => return Err(bad("has an escape other than \\\" or \\\\")),
                    },
                    Some(c) => key.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == '.' || c == '[' {
                    break;
                }
                if c == ']' || c == '"' {
                    return Err(bad("has a bracket or quote inside an unquoted key"));
                }
                key.push(c);
                chars.next();
            }
        }
        if key.is_empty() {
            return Err(bad("has an empty key"));
        }
        let mut each = false;
        if chars.peek() == Some(&'[') {
            chars.next();
            if chars.next() != Some('*') || chars.next() != Some(']') {
                return Err(bad("has a bracket other than [*]"));
            }
            each = true;
        }
        segments.push(Segment { key, each });
        match chars.next() {
            None => return Ok(segments),
            Some('.') => continue,
            Some(_) => return Err(bad("has text after [*] that is not a dot")),
        }
    }
}

fn walk(value: &Value, path: &[Segment], out: &mut Vec<String>) {
    let Some((seg, rest)) = path.split_first() else {
        match value {
            Value::String(s) => out.push(s.clone()),
            Value::Number(n) => out.push(n.to_string()),
            Value::Bool(b) => out.push(b.to_string()),
            Value::Null | Value::Array(_) | Value::Object(_) => {}
        }
        return;
    };
    let Some(next) = value.as_object().and_then(|o| o.get(&seg.key)) else {
        return;
    };
    if seg.each {
        if let Value::Array(items) = next {
            for item in items {
                walk(item, rest, out);
            }
        }
    } else {
        walk(next, rest, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(rules: Vec<ClaimRule>) -> Provider {
        Provider {
            name: "corp".into(),
            issuer: "https://login.example.org/".into(),
            audience: "tessera".into(),
            jwks_url: "https://login.example.org/keys".into(),
            rules,
            role_mappings: Vec::new(),
        }
    }

    fn rule(claim: &str, template: &str) -> ClaimRule {
        ClaimRule {
            claim: claim.into(),
            template: template.into(),
        }
    }

    fn mapping(term: &str, group: &str) -> RoleMapping {
        RoleMapping {
            term: term.into(),
            group: group.into(),
        }
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_bare_value_template_passes_each_value_through_as_a_term() {
        let p = provider(vec![rule("groups[*]", "{value}")]);
        assert!(p.validated(false).is_ok());
        let m = p.apply(&json!({"sub": "u1", "groups": ["analysts", " eu "]}));
        assert_eq!(m.terms, set(&["analysts", "eu"]));
        assert!(m.groups.is_empty());
    }

    #[test]
    fn a_template_with_literal_text_adds_it_to_each_value() {
        let p = provider(vec![rule("groups[*]", "group:{value}")]);
        let m = p.apply(&json!({"groups": ["analysts", "eu"]}));
        assert_eq!(m.terms, set(&["group:analysts", "group:eu"]));
    }

    #[test]
    fn nested_and_quoted_paths_reach_their_values() {
        let p = provider(vec![
            rule("realm_access.roles[*]", "role:{value}"),
            rule(
                r#""https://example.org/claims".teams[*].id"#,
                "team:{value}",
            ),
            rule("email_verified", "verified"),
            rule("level", "level-{value}"),
        ]);
        let m = p.apply(&json!({
            "realm_access": {"roles": ["viewer", "editor"]},
            "https://example.org/claims": {"teams": [{"id": 1}, {"id": "b"}, {"name": "x"}]},
            "email_verified": true,
            "level": 3
        }));
        let want = [
            "role:viewer",
            "role:editor",
            "team:1",
            "team:b",
            "verified",
            "level-3",
        ];
        assert_eq!(m.terms, set(&want));
    }

    #[test]
    fn a_path_that_does_not_match_the_claims_shape_produces_nothing() {
        let p = provider(vec![
            rule("groups", "g:{value}"),
            rule("tid[*]", "t:{value}"),
            rule("missing", "m:{value}"),
            rule("obj", "o:{value}"),
            rule("nothing", "n:{value}"),
        ]);
        let m = p.apply(&json!({
            "groups": ["a"], "tid": "x", "obj": {"a": 1}, "nothing": null
        }));
        assert!(m.terms.is_empty(), "{:?}", m.terms);
    }

    #[test]
    fn a_value_producing_public_in_any_case_a_control_character_or_nothing_is_dropped() {
        let p = provider(vec![rule("groups[*]", "{value}")]);
        let claims =
            json!({"groups": ["public", "PUBLIC", " Public ", "  ", "", " kept ", "a\u{7}"]});
        assert_eq!(p.apply(&claims).terms, set(&["kept"]));
    }

    #[test]
    fn a_role_mapping_names_its_group_only_for_its_exact_term() {
        let mut p = provider(vec![rule("groups[*]", "{value}")]);
        p.role_mappings = vec![mapping("tessera-admins", "admins")];
        let m = p.apply(&json!({"groups": ["analysts", " tessera-admins "]}));
        assert_eq!(m.groups, set(&["admins"]));
        assert_eq!(m.terms, set(&["analysts", "tessera-admins"]));
        for other in [
            json!({"groups": ["tessera-admins-x", "Tessera-Admins", "tessera-admin"]}),
            json!({"groups": ["admins"]}),
            json!({}),
        ] {
            assert!(p.apply(&other).groups.is_empty(), "{other}");
        }
    }

    #[test]
    fn a_role_mapping_matches_the_produced_term_and_not_the_claim_value() {
        let mut p = provider(vec![rule("groups[*]", "group:{value}")]);
        p.role_mappings = vec![
            mapping("tessera-admins", "admins"),
            mapping("group:ops", "ops"),
            mapping("group:ops", "oncall"),
        ];
        let m = p.apply(&json!({"groups": ["tessera-admins", "ops"]}));
        assert_eq!(m.groups, set(&["ops", "oncall"]));
    }

    fn refused(p: Provider) {
        assert!(p.validated(false).is_err(), "{p:?} was accepted");
    }

    #[test]
    fn a_malformed_claim_path_is_refused() {
        for path in ["", "a..b", "a[0]", "a[*]b", "\"unclosed"] {
            refused(provider(vec![rule(path, "x")]));
        }
    }

    #[test]
    fn a_template_that_cannot_produce_a_term_is_refused() {
        for t in ["public", "PUBLIC", "  ", "{value}-{value}", "g\u{0}{value}"] {
            refused(provider(vec![rule("groups[*]", t)]));
        }
    }

    #[test]
    fn a_role_mapping_to_a_term_or_group_that_cannot_be_stored_is_refused() {
        let bad = [
            (" ", "admins"),
            ("public", "admins"),
            ("Public", "admins"),
            ("a\u{7}b", "admins"),
            ("tessera-admins", ""),
            ("tessera-admins", "ad\tmins"),
            ("tessera-admins", "ad\u{202E}mins"),
        ];
        for (term, group) in bad {
            let mut p = provider(vec![]);
            p.role_mappings = vec![mapping(term, group)];
            refused(p);
        }
    }

    #[test]
    fn rules_and_role_mappings_are_stored_trimmed() {
        let mut p = provider(vec![rule(" groups[*] ", " group:{value} ")]);
        p.role_mappings = vec![mapping(" tessera-admins ", " admins ")];
        let v = p.validated(false).unwrap();
        assert_eq!(v.rules, vec![rule("groups[*]", "group:{value}")]);
        assert_eq!(v.role_mappings, vec![mapping("tessera-admins", "admins")]);
    }

    #[test]
    fn a_blank_or_malformed_field_is_refused() {
        for blank in ["name", "issuer", "audience", "control", "bidi"] {
            let mut p = provider(vec![]);
            match blank {
                "name" => p.name = " ".into(),
                "issuer" => p.issuer = String::new(),
                "audience" => p.audience = "\t".into(),
                "bidi" => p.name = "co\u{202E}rp".into(),
                _ => p.name = "co\u{1b}rp".into(),
            }
            assert!(p.validated(false).is_err(), "blank {blank} accepted");
        }
    }

    fn with_jwks(url: &str) -> Provider {
        Provider {
            jwks_url: url.into(),
            ..provider(vec![])
        }
    }

    #[test]
    fn a_jwks_url_is_https_or_http_to_a_loopback_address() {
        for ok in [
            "https://login.example.org/keys",
            "HTTPS://login.example.org/keys",
            "http://localhost:8080/keys",
            "http://127.0.0.1/keys",
            "http://[::1]:9000/keys",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://LocalHost?x",
        ] {
            assert!(with_jwks(ok).validated(false).is_ok(), "{ok} refused");
        }
        let insecure = [
            "http://login.example.org/keys",
            "http://localhost@evil.example.org/keys",
            "http://localhost:80@evil.example.org/",
            "http://localhost@evil.org/",
            "http://localhost.evil.example.org/",
            "http://127.0.0.2/keys",
            "http://[::2]/keys",
        ];
        for bad in insecure {
            assert!(
                matches!(
                    with_jwks(bad).validated(false),
                    Err(Error::InsecureJwks { .. })
                ),
                "{bad} accepted"
            );
            assert!(with_jwks(bad).validated(true).is_ok(), "{bad} refused");
        }
    }

    #[test]
    fn a_jwks_url_that_does_not_parse_or_is_not_http_is_refused_even_when_insecure_is_allowed() {
        for never in [
            "login.example.org/keys",
            "ftp://localhost/keys",
            "https://",
            "http://[::1]evil.org/",
            "http://[::1].evil.org",
            "http://localhost:80:evil.org",
        ] {
            assert!(
                with_jwks(never).validated(true).is_err(),
                "{never} accepted"
            );
        }
    }
}
