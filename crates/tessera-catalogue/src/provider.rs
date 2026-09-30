//! OIDC providers as stored data, and the claim rules that turn a token's claims into terms and
//! local groups. Validating a token's signature, issuer, audience and lifetime happens elsewhere;
//! this module sees claims that have already been accepted.
//!
//! A claim path is a dot-separated list of object keys. A key followed by `[*]` takes every
//! element of the array found there. A key that itself holds a dot or a bracket is written in
//! double quotes, with `\"` and `\\` as escapes: `"https://example.org/roles"[*]`.
//!
//! A template produces a term or a local group name from each value the path reaches. It holds
//! `{value}` at most once, replaced by the value. A term template may be `{value}` alone, which
//! passes each value through as a term. A local group template that holds `{value}` also holds
//! literal text, so a claim value cannot name a local group made by hand. A template without
//! `{value}` produces itself whenever the path reaches any value. Values are strings, numbers and booleans; the path reaching an object, an
//! array without `[*]`, or null produces nothing.
//!
//! A template reaches terms only: those it produces, or those granted to the local group it
//! produces. Permissions reach an OIDC identity only through a rule that names a fixed local group
//! and the claim value it requires, so a claim value can select a group an administrator named
//! and cannot choose one by its own spelling.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::names;
use crate::Error;

const PLACEHOLDER: &str = "{value}";

/// An OIDC provider: where its tokens come from, who they are for, where its signing keys are
/// published, and how its claims map to terms and local groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub name: String,
    pub issuer: String,
    pub audience: String,
    pub jwks_url: String,
    pub rules: Vec<ClaimRule>,
}

/// Reads the claim at `claim` and produces what `target` names from each value there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRule {
    pub claim: String,
    pub target: RuleTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleTarget {
    /// A template for a term.
    Term(String),
    /// A template for the name of a local group, whose granted terms are added.
    LocalGroup(String),
    /// The local group `group`, whose granted terms and permissions are added when the claim
    /// holds the value `equals`.
    FixedGroup { equals: String, group: String },
}

/// What a provider's claim rules produce from one set of claims, before local groups are
/// expanded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaimMapping {
    pub terms: BTreeSet<String>,
    /// Local groups whose terms are added.
    pub groups: BTreeSet<String>,
    /// Local groups whose terms and permissions are added.
    pub fixed_groups: BTreeSet<String>,
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
        Ok(Provider {
            name,
            issuer,
            audience,
            jwks_url,
            rules,
        })
    }

    /// Applies every claim rule to `claims`, a JSON object. A value producing a term or a group
    /// name that a grant or a group could not hold is dropped. That includes `public` in any
    /// case, which every session holds anyway.
    pub fn apply(&self, claims: &Value) -> ClaimMapping {
        let mut out = ClaimMapping::default();
        for rule in &self.rules {
            // A stored rule's path has been parsed once already; one that fails here was not
            // validated and produces nothing.
            let Ok(path) = parse_path(&rule.claim) else {
                continue;
            };
            let mut values = Vec::new();
            walk(claims, &path, &mut values);
            for value in values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
                rule.target.produce(value, &mut out);
            }
        }
        out
    }

    /// Whether any of this provider's rules could add the terms of the local group `group`.
    pub(crate) fn may_reach_group_terms(&self, group: &str) -> bool {
        self.rules.iter().any(|r| match &r.target {
            RuleTarget::LocalGroup(t) => template_may_produce(t, group),
            RuleTarget::FixedGroup { group: g, .. } => g == group,
            RuleTarget::Term(_) => false,
        })
    }

    /// Whether any of this provider's rules could add the permissions of the local group `group`.
    pub(crate) fn may_reach_group_permissions(&self, group: &str) -> bool {
        self.rules
            .iter()
            .any(|r| matches!(&r.target, RuleTarget::FixedGroup { group: g, .. } if g == group))
    }
}

impl RuleTarget {
    fn produce(&self, value: &str, out: &mut ClaimMapping) {
        match self {
            RuleTarget::Term(t) => {
                if let Ok(term) = names::term(&t.replacen(PLACEHOLDER, value, 1)) {
                    out.terms.insert(term);
                }
            }
            RuleTarget::LocalGroup(t) => {
                if let Ok(group) = names::name("group", &t.replacen(PLACEHOLDER, value, 1)) {
                    out.groups.insert(group);
                }
            }
            RuleTarget::FixedGroup { equals, group } => {
                if value == equals {
                    out.fixed_groups.insert(group.clone());
                }
            }
        }
    }
}

impl ClaimRule {
    fn validated(&self) -> Result<ClaimRule, Error> {
        let claim = self.claim.trim().to_owned();
        parse_path(&claim)?;
        let target = match &self.target {
            RuleTarget::Term(t) => {
                let t = template(t)?;
                names::term(&t)?;
                RuleTarget::Term(t)
            }
            RuleTarget::LocalGroup(t) => {
                let t = template(t)?;
                if t == PLACEHOLDER {
                    return Err(Error::Invalid(
                        "the local group template `{value}` holds no literal text, so a claim \
                         value could name any local group; write a prefix or suffix beside it, \
                         such as `tenant-{value}`, or name a fixed local group with the claim \
                         value it requires"
                            .into(),
                    ));
                }
                names::name("local group template", &t)?;
                RuleTarget::LocalGroup(t)
            }
            RuleTarget::FixedGroup { equals, group } => fixed_group(equals, group)?,
        };
        Ok(ClaimRule { claim, target })
    }
}

/// A trimmed template, refused when it holds `{value}` more than once.
fn template(raw: &str) -> Result<String, Error> {
    let t = raw.trim();
    if t.matches(PLACEHOLDER).count() > 1 {
        return Err(Error::Invalid(format!(
            "the template `{t}` holds {{value}} more than once; write it at most once"
        )));
    }
    Ok(t.to_owned())
}

fn fixed_group(equals: &str, group: &str) -> Result<RuleTarget, Error> {
    let group = names::name("local group", group)?;
    let equals = equals.trim();
    if equals.is_empty() {
        return Err(Error::Invalid(format!(
            "the rule naming local group `{group}` requires an empty claim value, which no claim \
             holds; write the value the claim must hold"
        )));
    }
    Ok(RuleTarget::FixedGroup {
        equals: equals.to_owned(),
        group,
    })
}

/// Refuses a JWKS URL other than `https://`, or `http://` to `localhost`, `127.0.0.1` or
/// `[::1]`, unless `allow_insecure` is set. Anyone on the network path of a plain `http` fetch
/// could substitute the keys and sign a token for any identity.
fn check_jwks_url(provider: &str, url: &str, allow_insecure: bool) -> Result<(), Error> {
    let bad = |why: &str| {
        Error::Invalid(format!(
            "provider `{provider}` has JWKS URL `{url}`, {why}; write an https:// URL, or an \
             http:// URL to localhost, 127.0.0.1 or [::1]"
        ))
    };
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(bad("which is not an http:// or https:// URL"));
    };
    let host = url_host(rest);
    if host.is_empty() {
        return Err(bad("which names no host"));
    }
    if secure || allow_insecure || is_loopback(host) {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "provider `{provider}` has JWKS URL `{url}`, which fetches the signing keys over plain \
         http from a host that is not a loopback address; write an https:// URL, or open the \
         catalogue with insecure JWKS URLs allowed (TESSERA_ALLOW_INSECURE_JWKS=1)"
    )))
}

/// The host of a URL whose scheme and `://` are removed: the authority ends at the first `/`,
/// `\`, `?` or `#`, the host follows the last `@` in it, and a port is removed.
fn url_host(rest: &str) -> &str {
    let authority = rest.split(['/', '\\', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    match host.find(']') {
        Some(end) if host.starts_with('[') => &host[..=end],
        _ => host.split(':').next().unwrap_or_default(),
    }
}

fn is_loopback(host: &str) -> bool {
    ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .any(|l| host.eq_ignore_ascii_case(l))
}

fn template_may_produce(template: &str, produced: &str) -> bool {
    match template.split_once(PLACEHOLDER) {
        None => template == produced,
        Some((pre, suf)) => {
            produced.len() > pre.len() + suf.len()
                && produced.starts_with(pre)
                && produced.ends_with(suf)
        }
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
        }
    }

    fn term_rule(claim: &str, t: &str) -> ClaimRule {
        ClaimRule {
            claim: claim.into(),
            target: RuleTarget::Term(t.into()),
        }
    }

    fn group_rule(claim: &str, t: &str) -> ClaimRule {
        ClaimRule {
            claim: claim.into(),
            target: RuleTarget::LocalGroup(t.into()),
        }
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn each_element_of_a_groups_claim_becomes_a_term() {
        let p = provider(vec![term_rule("groups[*]", "group:{value}")]);
        let m = p.apply(&json!({"sub": "u1", "groups": ["analysts", "eu"]}));
        assert_eq!(m.terms, set(&["group:analysts", "group:eu"]));
        assert!(m.groups.is_empty());
    }

    #[test]
    fn a_tenant_claim_names_a_local_group() {
        let p = provider(vec![group_rule("tid", "tenant-{value}")]);
        let m = p.apply(&json!({"tid": "7f3a"}));
        assert_eq!(m.groups, set(&["tenant-7f3a"]));
        assert!(m.terms.is_empty());
    }

    #[test]
    fn nested_and_quoted_paths_reach_their_values() {
        let p = provider(vec![
            term_rule("realm_access.roles[*]", "role:{value}"),
            term_rule(
                r#""https://example.org/claims".teams[*].id"#,
                "team:{value}",
            ),
            term_rule("email_verified", "verified"),
            term_rule("level", "level-{value}"),
        ]);
        let m = p.apply(&json!({
            "realm_access": {"roles": ["viewer", "editor"]},
            "https://example.org/claims": {"teams": [{"id": 1}, {"id": "b"}, {"name": "x"}]},
            "email_verified": true,
            "level": 3
        }));
        assert_eq!(
            m.terms,
            set(&[
                "role:viewer",
                "role:editor",
                "team:1",
                "team:b",
                "verified",
                "level-3"
            ])
        );
    }

    #[test]
    fn a_path_that_does_not_match_the_claims_shape_produces_nothing() {
        let p = provider(vec![
            term_rule("groups", "g:{value}"),
            term_rule("tid[*]", "t:{value}"),
            term_rule("missing", "m:{value}"),
            term_rule("obj", "o:{value}"),
            term_rule("nothing", "n:{value}"),
        ]);
        let m = p.apply(&json!({
            "groups": ["a"], "tid": "x", "obj": {"a": 1}, "nothing": null
        }));
        assert!(m.terms.is_empty(), "{:?}", m.terms);
    }

    fn fixed_rule(claim: &str, equals: &str, group: &str) -> ClaimRule {
        ClaimRule {
            claim: claim.into(),
            target: RuleTarget::FixedGroup {
                equals: equals.into(),
                group: group.into(),
            },
        }
    }

    #[test]
    fn a_bare_value_template_passes_each_value_through_as_a_term() {
        let p = provider(vec![term_rule("groups[*]", "{value}")]);
        assert!(p.validated(false).is_ok());
        let m = p.apply(&json!({"groups": ["analysts", " eu "]}));
        assert_eq!(m.terms, set(&["analysts", "eu"]));
    }

    #[test]
    fn a_value_producing_public_in_any_case_a_control_character_or_nothing_is_dropped() {
        let p = provider(vec![
            term_rule("groups[*]", "{value}"),
            group_rule("groups[*]", "tenant-{value}"),
        ]);
        let claims =
            json!({"groups": ["public", "PUBLIC", " Public ", "  ", "", " kept ", "a\u{7}"]});
        let m = p.apply(&claims);
        assert_eq!(m.terms, set(&["kept"]));
        assert_eq!(
            m.groups,
            set(&[
                "tenant-kept",
                "tenant-public",
                "tenant-PUBLIC",
                "tenant-Public"
            ])
        );
    }

    #[test]
    fn a_fixed_group_rule_names_its_group_only_when_the_claim_holds_its_value() {
        let p = provider(vec![fixed_rule("groups[*]", "tessera-admins", "admins")]);
        let m = p.apply(&json!({"groups": ["analysts", " tessera-admins "]}));
        assert_eq!(m.fixed_groups, set(&["admins"]));
        assert!(m.groups.is_empty() && m.terms.is_empty());
        for other in [json!({"groups": ["tessera-admin", "admins"]}), json!({})] {
            assert!(p.apply(&other).fixed_groups.is_empty(), "{other}");
        }
    }

    #[test]
    fn a_malformed_rule_is_refused() {
        let bad_rules = [
            term_rule("", "x"),
            term_rule("a..b", "x"),
            term_rule("a[0]", "x"),
            term_rule("a[*]b", "x"),
            term_rule(r#""unclosed"#, "x"),
            term_rule("groups[*]", "public"),
            term_rule("groups[*]", "PUBLIC"),
            term_rule("groups[*]", "  "),
            term_rule("groups[*]", "{value}-{value}"),
            term_rule("groups[*]", "g\u{0}{value}"),
            group_rule("tid", ""),
            group_rule("tid", "{value}"),
            group_rule("tid", " {value} "),
            group_rule("tid", "t\n{value}"),
            fixed_rule("groups[*]", " ", "admins"),
            fixed_rule("groups[*]", "tessera-admins", ""),
            fixed_rule("groups[*]", "tessera-admins", "ad\tmins"),
        ];
        for rule in bad_rules {
            assert!(
                provider(vec![rule.clone()]).validated(false).is_err(),
                "{rule:?} was accepted"
            );
        }
        let good = provider(vec![fixed_rule(
            " groups[*] ",
            " tessera-admins ",
            " admins ",
        )]);
        assert_eq!(
            good.validated(false).unwrap().rules,
            vec![fixed_rule("groups[*]", "tessera-admins", "admins")]
        );
    }

    #[test]
    fn a_blank_or_malformed_field_is_refused() {
        for blank in ["name", "issuer", "audience", "control"] {
            let mut p = provider(vec![]);
            match blank {
                "name" => p.name = " ".into(),
                "issuer" => p.issuer = String::new(),
                "audience" => p.audience = "\t".into(),
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
            "http://localhost:8080/keys",
            "http://127.0.0.1/keys",
            "http://[::1]:9000/keys",
            "http://LocalHost?x",
        ] {
            assert!(with_jwks(ok).validated(false).is_ok(), "{ok} refused");
        }
        let insecure = [
            "http://login.example.org/keys",
            "http://localhost@evil.example.org/keys",
            "http://localhost:80@evil.example.org/",
            "http://localhost.evil.example.org/",
            "http://127.0.0.2/keys",
            "http://[::2]/keys",
        ];
        for bad in insecure {
            assert!(with_jwks(bad).validated(false).is_err(), "{bad} accepted");
            assert!(with_jwks(bad).validated(true).is_ok(), "{bad} refused");
        }
        for never in [
            "login.example.org/keys",
            "ftp://localhost/keys",
            "https://",
            "http:///k",
        ] {
            assert!(
                with_jwks(never).validated(true).is_err(),
                "{never} accepted"
            );
        }
    }

    #[test]
    fn only_a_fixed_group_rule_reaches_a_groups_permissions() {
        let p = provider(vec![
            group_rule("tid", "tenant-{value}"),
            group_rule("x", "ops"),
            fixed_rule("groups[*]", "tessera-admins", "admins"),
        ]);
        for group in ["tenant-7f3a", "ops", "admins"] {
            assert!(p.may_reach_group_terms(group), "{group}");
        }
        assert!(!p.may_reach_group_terms("tenant-"));
        assert!(!p.may_reach_group_terms("analysts"));
        assert!(p.may_reach_group_permissions("admins"));
        for group in ["tenant-7f3a", "ops", "analysts"] {
            assert!(!p.may_reach_group_permissions(group), "{group}");
        }
    }
}
