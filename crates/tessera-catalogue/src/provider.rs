//! OIDC providers as stored data, and the claim rules that turn a token's claims into terms and
//! local groups. Validating a token's signature, issuer, audience and lifetime happens elsewhere;
//! this module sees claims that have already been accepted.
//!
//! A claim path is a dot-separated list of object keys. A key followed by `[*]` takes every
//! element of the array found there. A key that itself holds a dot or a bracket is written in
//! double quotes, with `\"` and `\\` as escapes: `"https://example.org/roles"[*]`.
//!
//! A template produces a term or a local group name from each value the path reaches. It holds
//! `{value}` at most once, replaced by the value. A template without `{value}` produces itself
//! whenever the path reaches any value. Values are strings, numbers and booleans; the path
//! reaching an object, an array without `[*]`, or null produces nothing.

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
    /// A template for the name of a local group, whose granted terms and permissions are added.
    LocalGroup(String),
}

/// What a provider's claim rules produce from one set of claims, before local groups are
/// expanded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaimMapping {
    pub terms: BTreeSet<String>,
    pub groups: BTreeSet<String>,
}

impl Provider {
    /// The provider with every field trimmed, or the reason it cannot be stored.
    pub(crate) fn validated(&self) -> Result<Provider, Error> {
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
        if !(jwks_url.starts_with("https://") || jwks_url.starts_with("http://")) {
            return Err(Error::Invalid(format!(
                "provider `{name}` has JWKS URL `{jwks_url}`; write an http:// or https:// URL"
            )));
        }
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

    /// Applies every claim rule to `claims`, a JSON object. Values producing an empty term or
    /// group, or the term `public`, which every session holds anyway, are dropped.
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
            for value in values {
                let value = value.trim();
                if value.is_empty() {
                    continue;
                }
                let (template, sink) = match &rule.target {
                    RuleTarget::Term(t) => (t, &mut out.terms),
                    RuleTarget::LocalGroup(t) => (t, &mut out.groups),
                };
                let produced = template.replacen(PLACEHOLDER, value, 1);
                let produced = produced.trim();
                let is_term = matches!(rule.target, RuleTarget::Term(_));
                if produced.is_empty() || (is_term && produced == names::PUBLIC) {
                    continue;
                }
                sink.insert(produced.to_owned());
            }
        }
        out
    }

    /// Whether any of this provider's rules could produce the local group `group`.
    pub(crate) fn may_produce_group(&self, group: &str) -> bool {
        self.rules.iter().any(|r| match &r.target {
            RuleTarget::LocalGroup(t) => template_may_produce(t, group),
            RuleTarget::Term(_) => false,
        })
    }
}

impl ClaimRule {
    fn validated(&self) -> Result<ClaimRule, Error> {
        let claim = self.claim.trim().to_owned();
        parse_path(&claim)?;
        let template = |raw: &str| -> Result<String, Error> {
            let t = raw.trim();
            if t.matches(PLACEHOLDER).count() > 1 {
                return Err(Error::Invalid(format!(
                    "the template `{t}` holds {{value}} more than once; write it at most once"
                )));
            }
            Ok(t.to_owned())
        };
        let target = match &self.target {
            RuleTarget::Term(t) => {
                let t = template(t)?;
                // A template without `{value}` is a term itself, and takes a term's rules.
                if !t.contains(PLACEHOLDER) {
                    names::term(&t)?;
                }
                RuleTarget::Term(t)
            }
            RuleTarget::LocalGroup(t) => {
                let t = template(t)?;
                names::name("local group template", &t)?;
                RuleTarget::LocalGroup(t)
            }
        };
        Ok(ClaimRule { claim, target })
    }
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

    #[test]
    fn a_value_producing_public_or_nothing_is_dropped() {
        let p = provider(vec![
            term_rule("groups[*]", "{value}"),
            group_rule("groups[*]", "tenant-{value}"),
        ]);
        let m = p.apply(&json!({"groups": ["public", "  ", "", " kept "]}));
        assert_eq!(m.terms, set(&["kept"]));
        assert_eq!(m.groups, set(&["tenant-kept", "tenant-public"]));
    }

    #[test]
    fn a_malformed_rule_or_field_is_refused() {
        let bad_rules = [
            term_rule("", "x"),
            term_rule("a..b", "x"),
            term_rule("a[0]", "x"),
            term_rule("a[*]b", "x"),
            term_rule(r#""unclosed"#, "x"),
            term_rule("groups[*]", "public"),
            term_rule("groups[*]", "  "),
            term_rule("groups[*]", "{value}-{value}"),
            group_rule("tid", ""),
        ];
        for rule in bad_rules {
            assert!(
                provider(vec![rule.clone()]).validated().is_err(),
                "{rule:?} was accepted"
            );
        }
        let mut p = provider(vec![]);
        p.jwks_url = "login.example.org/keys".into();
        assert!(p.validated().is_err());
        for blank in ["name", "issuer", "audience"] {
            let mut p = provider(vec![]);
            match blank {
                "name" => p.name = " ".into(),
                "issuer" => p.issuer = String::new(),
                _ => p.audience = "\t".into(),
            }
            assert!(p.validated().is_err(), "blank {blank} accepted");
        }
    }

    #[test]
    fn a_group_template_matches_the_names_it_can_produce() {
        let p = provider(vec![
            group_rule("tid", "tenant-{value}"),
            group_rule("x", "ops"),
        ]);
        assert!(p.may_produce_group("tenant-7f3a"));
        assert!(p.may_produce_group("ops"));
        assert!(!p.may_produce_group("tenant-"));
        assert!(!p.may_produce_group("analysts"));
    }
}
