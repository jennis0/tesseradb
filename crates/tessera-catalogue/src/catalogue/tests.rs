use serde_json::json;

use super::*;
use crate::provider::{ClaimRule, RoleMapping};
use crate::testing::Fixture;

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn perms(ps: &[Permission]) -> PermissionSet {
    ps.iter().copied().collect()
}

fn corp(rules: Vec<ClaimRule>) -> Provider {
    Provider {
        name: "corp".into(),
        issuer: "https://login.example.org/".into(),
        audience: "tessera".into(),
        jwks_url: "https://login.example.org/keys".into(),
        rules,
        role_mappings: Vec::new(),
    }
}

/// `groups[*] -> {value}`, which passes each group through as a term.
fn groups_rule() -> ClaimRule {
    ClaimRule {
        claim: "groups[*]".into(),
        template: "{value}".into(),
    }
}

fn tenant_rule() -> ClaimRule {
    ClaimRule {
        claim: "tid".into(),
        template: "tenant:{value}".into(),
    }
}

/// A role mapping on the `groups` claim.
fn mapping(value: &str, group: &str) -> RoleMapping {
    RoleMapping {
        claim: "groups[*]".into(),
        value: value.into(),
        group: group.into(),
    }
}

/// `corp` with `groups_rule` and the role mapping `groups[*]: tessera-admins -> admins`.
fn corp_with_admins() -> Provider {
    Provider {
        role_mappings: vec![mapping("tessera-admins", "admins")],
        ..corp(vec![groups_rule()])
    }
}

#[test]
fn a_principal_resolves_to_its_own_grants_and_those_of_its_groups() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_principal("ada", PrincipalKind::Person).unwrap();
    cat.create_group("eu").unwrap();
    cat.create_group("ops").unwrap();
    cat.grant_term(Grantee::Principal("ada"), "secret").unwrap();
    cat.grant_term(Grantee::Group("eu"), "region:eu").unwrap();
    cat.grant_term(Grantee::Group("ops"), "ops").unwrap();
    cat.grant_permission(Grantee::Group("eu"), Permission::Read)
        .unwrap();
    cat.add_member("eu", "ada").unwrap();

    let r = cat.resolve("ada", None).unwrap();
    assert_eq!(r.terms, set(&["secret", "region:eu"]));
    assert_eq!(r.permissions, perms(&[Permission::Read]));
    assert!(!r.bypass);

    cat.remove_member("eu", "ada").unwrap();
    cat.add_member("ops", "ada").unwrap();
    let r = cat.resolve("ada", None).unwrap();
    assert_eq!(r.terms, set(&["secret", "ops"]));
    assert_eq!(r.permissions, PermissionSet::EMPTY);

    cat.delete_group("ops").unwrap();
    assert_eq!(cat.resolve("ada", None).unwrap().terms, set(&["secret"]));
    assert!(cat.principal("ada").unwrap().groups.is_empty());

    cat.revoke_term(Grantee::Principal("ada"), " secret ")
        .unwrap();
    assert!(cat.resolve("ada", None).unwrap().terms.is_empty());
}

#[test]
fn each_permission_is_granted_alone_and_admin_implies_nothing() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_principal("root", PrincipalKind::Person).unwrap();
    for p in Permission::ALL {
        cat.grant_permission(Grantee::Principal("root"), p).unwrap();
        assert_eq!(cat.resolve("root", None).unwrap().permissions, perms(&[p]));
        cat.revoke_permission(Grantee::Principal("root"), p)
            .unwrap();
    }
    cat.set_bypass("root", true).unwrap();
    let r = cat.resolve("root", None).unwrap();
    assert!(r.bypass);
    assert_eq!(r.permissions, PermissionSet::EMPTY);
}

#[test]
fn a_disabled_or_deleted_principal_resolves_to_nothing() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_principal("ada", PrincipalKind::Person).unwrap();
    cat.create_group("eu").unwrap();
    cat.add_member("eu", "ada").unwrap();
    cat.disable_principal("ada").unwrap();
    assert_eq!(cat.resolve("ada", None), None);
    assert!(cat.principal("ada").unwrap().disabled);
    cat.enable_principal("ada").unwrap();
    assert!(cat.resolve("ada", None).is_some());

    let (key, _) = cat.create_api_key("ada", None, None).unwrap();
    cat.delete_principal("ada").unwrap();
    assert_eq!(cat.resolve("ada", None), None);
    assert!(cat.group("eu").unwrap().members.is_empty());
    assert!(cat.api_keys("ada").is_empty());
    assert!(cat.verify_api_key(&key.key).is_err());
    for missing in [
        cat.delete_principal("ada"),
        cat.disable_principal("ada"),
        cat.add_member("eu", "ada"),
        cat.grant_term(Grantee::Principal("ada"), "x"),
        cat.grant_term(Grantee::Group("nope"), "x"),
        cat.grant_permission(Grantee::Group("nope"), Permission::Read),
        cat.delete_group("nope"),
    ] {
        assert!(
            matches!(missing, Err(Error::NotFound { .. })),
            "{missing:?}"
        );
    }

    // A principal created again under the name starts with nothing.
    cat.create_principal("ada", PrincipalKind::Service).unwrap();
    let info = cat.principal("ada").unwrap();
    assert_eq!(info.kind, PrincipalKind::Service);
    assert!(info.groups.is_empty() && info.terms.is_empty());
}

#[test]
fn an_oidc_identity_holds_the_terms_its_claim_rules_produce_and_nothing_else() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_provider(&corp(vec![groups_rule(), tenant_rule()]))
        .unwrap();
    // A local group named as a claim value, with no role mapping, passes on nothing.
    cat.create_group("analysts").unwrap();
    cat.grant_term(Grantee::Group("analysts"), "secret")
        .unwrap();
    cat.grant_permission(Grantee::Group("analysts"), Permission::Admin)
        .unwrap();

    let claims = json!({"sub": "u1", "groups": ["analysts", "eu"], "tid": "7f3a"});
    let r = cat.resolve_claims("corp", &claims).unwrap();
    assert_eq!(r.terms, set(&["analysts", "eu", "tenant:7f3a"]));
    assert_eq!(r.permissions, PermissionSet::EMPTY);
    assert!(!r.bypass);
    assert_eq!(cat.resolve_claims("nobody", &claims), None);
}

#[test]
fn a_role_mapping_passes_on_its_groups_terms_and_permissions_and_never_bypass() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_provider(&corp_with_admins()).unwrap();
    let admin = json!({"groups": ["analysts", "tessera-admins"]});
    // A mapping to a group that does not exist passes on nothing.
    let r = cat.resolve_claims("corp", &admin).unwrap();
    assert_eq!(r.terms, set(&["analysts", "tessera-admins"]));
    assert_eq!(r.permissions, PermissionSet::EMPTY);

    cat.create_group("admins").unwrap();
    cat.grant_term(Grantee::Group("admins"), "ops").unwrap();
    cat.grant_permission(Grantee::Group("admins"), Permission::Admin)
        .unwrap();
    let r = cat.resolve_claims("corp", &admin).unwrap();
    assert_eq!(r.terms, set(&["analysts", "tessera-admins", "ops"]));
    assert_eq!(r.permissions, perms(&[Permission::Admin]));
    assert!(!r.bypass);
}

#[test]
fn a_claim_value_that_does_not_exactly_match_a_role_mapping_passes_on_nothing() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_provider(&corp_with_admins()).unwrap();
    cat.create_group("admins").unwrap();
    cat.grant_term(Grantee::Group("admins"), "ops").unwrap();
    cat.grant_permission(Grantee::Group("admins"), Permission::Admin)
        .unwrap();
    for value in [
        "tessera-admins-x",
        "Tessera-Admins",
        "tessera-admin",
        "admins",
    ] {
        let r = cat
            .resolve_claims("corp", &json!({ "groups": [value] }))
            .unwrap();
        assert_eq!(r.terms, set(&[value]), "{value}");
        assert_eq!(r.permissions, PermissionSet::EMPTY, "{value}");
    }
}

#[test]
fn the_mapped_value_in_another_claim_passes_on_nothing_beyond_its_term() {
    let fx = Fixture::new();
    let cat = fx.open();
    let department = ClaimRule {
        claim: "department".into(),
        template: "{value}".into(),
    };
    cat.create_provider(&Provider {
        rules: vec![groups_rule(), department],
        ..corp_with_admins()
    })
    .unwrap();
    cat.create_group("admins").unwrap();
    cat.grant_permission(Grantee::Group("admins"), Permission::Admin)
        .unwrap();
    let r = cat
        .resolve_claims("corp", &json!({"department": "tessera-admins"}))
        .unwrap();
    assert_eq!(r.terms, set(&["tessera-admins"]));
    assert_eq!(r.permissions, PermissionSet::EMPTY);
}

#[test]
fn a_provider_is_created_changed_and_dropped_and_survives_reopening() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_provider(&corp(vec![groups_rule()])).unwrap();
    assert!(matches!(
        cat.create_provider(&corp(vec![])),
        Err(Error::Exists { .. })
    ));
    let mut changed = corp(vec![tenant_rule(), groups_rule()]);
    changed.audience = "  tessera-prod ".into();
    changed.role_mappings = vec![mapping(" tessera-admins ", "admins"), mapping("x", "y")];
    cat.update_provider(&changed).unwrap();
    drop(cat);

    let cat = fx.open();
    let stored = cat.provider("corp").unwrap();
    assert!(!stored.read_only);
    assert_eq!(stored.provider.audience, "tessera-prod");
    assert_eq!(stored.provider.rules, vec![tenant_rule(), groups_rule()]);
    assert_eq!(
        stored.provider.role_mappings,
        vec![mapping("tessera-admins", "admins"), mapping("x", "y")]
    );

    let mut bad = corp(vec![]);
    bad.jwks_url = "keys".into();
    assert!(matches!(cat.update_provider(&bad), Err(Error::Invalid(_))));
    assert!(matches!(cat.create_provider(&bad), Err(Error::Invalid(_))));

    cat.drop_provider("corp").unwrap();
    assert!(cat.providers().is_empty());
    assert!(matches!(
        cat.drop_provider("corp"),
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        cat.update_provider(&corp(vec![])),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn a_configured_provider_is_listed_and_cannot_be_changed_here() {
    let fx = Fixture::new();
    let mut options = fx.options();
    options.config_providers = vec![corp_with_admins()];
    let cat = Catalogue::open(&fx.path(), options.clone()).unwrap();
    let mut other = corp(vec![]);
    other.name = "partner".into();
    cat.create_provider(&other).unwrap();

    let listed: Vec<(String, bool)> = cat
        .providers()
        .into_iter()
        .map(|p| (p.provider.name, p.read_only))
        .collect();
    assert_eq!(
        listed,
        vec![("corp".into(), true), ("partner".into(), false)]
    );
    let claims = json!({"groups": ["a"]});
    assert_eq!(
        cat.resolve_claims("corp", &claims).unwrap().terms,
        set(&["a"])
    );

    for refused in [
        cat.create_provider(&corp(vec![])),
        cat.update_provider(&corp(vec![groups_rule()])),
        cat.drop_provider("corp"),
    ] {
        assert!(
            matches!(refused, Err(Error::ReadOnly { .. })),
            "{refused:?}"
        );
    }
    drop(cat);

    // Opened without the configuration, the catalogue holds only its own provider.
    let cat = fx.open();
    let names: Vec<String> = cat
        .providers()
        .into_iter()
        .map(|p| p.provider.name)
        .collect();
    assert_eq!(names, vec!["partner".to_owned()]);
    drop(cat);

    options.config_providers.push(other);
    assert!(matches!(
        Catalogue::open(&fx.path(), options.clone()),
        Err(Error::DeclaredTwice { .. })
    ));
    options.config_providers = vec![corp(vec![]), corp(vec![])];
    assert!(matches!(
        Catalogue::open(&fx.path(), options.clone()),
        Err(Error::DeclaredTwice { .. })
    ));
    let mut invalid = corp(vec![]);
    invalid.issuer = String::new();
    options.config_providers = vec![invalid];
    assert!(matches!(
        Catalogue::open(&fx.path(), options),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn a_configured_provider_with_a_plain_http_jwks_url_needs_insecure_jwks_allowed() {
    let fx = Fixture::new();
    let mut options = fx.options();
    options.config_providers = vec![Provider {
        jwks_url: "http://keys.internal/jwks".into(),
        ..corp(vec![])
    }];
    assert!(matches!(
        Catalogue::open(&fx.path(), options.clone()),
        Err(Error::InsecureJwks { .. })
    ));
    options.allow_insecure_jwks = true;
    let cat = Catalogue::open(&fx.path(), options).unwrap();
    assert!(cat.provider("corp").unwrap().read_only);
}

#[test]
fn a_declared_provider_with_a_plain_http_jwks_url_needs_insecure_jwks_allowed() {
    let fx = Fixture::new();
    let insecure = Provider {
        jwks_url: "http://keys.internal/jwks".into(),
        ..corp(vec![])
    };
    let cat = fx.open();
    assert!(matches!(
        cat.create_provider(&insecure),
        Err(Error::InsecureJwks { .. })
    ));
    cat.create_provider(&corp(vec![])).unwrap();
    assert!(matches!(
        cat.update_provider(&insecure),
        Err(Error::InsecureJwks { .. })
    ));
    drop(cat);
    let mut options = fx.options();
    options.allow_insecure_jwks = true;
    let cat = Catalogue::open(&fx.path(), options).unwrap();
    cat.update_provider(&insecure).unwrap();
    drop(cat);
    // Stored, it is refused by a catalogue opened without the allowance.
    assert!(matches!(
        Catalogue::open(&fx.path(), fx.options()),
        Err(Error::InsecureJwks { .. })
    ));
}

/// Strips the generation, which the tests below do not compare.
fn who(a: Affected) -> Affected {
    Affected { generation: 0, ..a }
}

fn principals(ps: &[&str]) -> Affected {
    Affected {
        principals: set(ps),
        ..Affected::default()
    }
}

fn providers(ps: &[&str]) -> Affected {
    Affected {
        providers: set(ps),
        ..Affected::default()
    }
}

/// A catalogue holding `ada`, `bob` and `cy`, with `ada` and `bob` in `eu`.
fn three_principals(fx: &Fixture) -> Catalogue {
    let cat = fx.open();
    for name in ["ada", "bob", "cy"] {
        cat.create_principal(name, PrincipalKind::Person).unwrap();
    }
    cat.create_group("eu").unwrap();
    cat.add_member("eu", "ada").unwrap();
    cat.add_member("eu", "bob").unwrap();
    cat
}

#[test]
fn creating_and_membership_changes_report_whom_they_affect() {
    let fx = Fixture::new();
    let cat = fx.open();
    let none = Affected::default();
    assert_eq!(
        who(cat.create_principal("ada", PrincipalKind::Person).unwrap()),
        none
    );
    assert_eq!(
        who(cat.create_principal("bob", PrincipalKind::Person).unwrap()),
        none
    );
    let ada = principals(&["ada"]);
    assert_eq!(
        who(cat.set_password("ada", "a long enough password").unwrap()),
        ada
    );
    assert_eq!(
        who(cat.set_password("ada", "a long enough password").unwrap()),
        ada
    );
    assert_eq!(who(cat.clear_password("ada").unwrap()), ada);
    assert_eq!(who(cat.clear_password("ada").unwrap()), none);
    assert_eq!(who(cat.create_group("eu").unwrap()), none);
    assert_eq!(
        who(cat.add_member("eu", "ada").unwrap()),
        principals(&["ada"])
    );
    assert_eq!(who(cat.add_member("eu", "ada").unwrap()), none);
    assert_eq!(
        who(cat.add_member("eu", "bob").unwrap()),
        principals(&["bob"])
    );
    assert_eq!(
        who(cat.remove_member("eu", "bob").unwrap()),
        principals(&["bob"])
    );
    assert_eq!(who(cat.remove_member("eu", "bob").unwrap()), none);
}

#[test]
fn grants_and_principal_flags_report_whom_they_affect() {
    let fx = Fixture::new();
    let cat = three_principals(&fx);
    let none = Affected::default();
    let ada = Grantee::Principal("ada");
    let eu = Grantee::Group("eu");
    assert_eq!(who(cat.grant_term(ada, "x").unwrap()), principals(&["ada"]));
    assert_eq!(who(cat.grant_term(ada, "x").unwrap()), none);
    assert_eq!(
        who(cat.revoke_term(ada, "x").unwrap()),
        principals(&["ada"])
    );
    assert_eq!(who(cat.revoke_term(ada, "x").unwrap()), none);
    assert_eq!(
        who(cat.grant_term(eu, "x").unwrap()),
        principals(&["ada", "bob"])
    );
    assert_eq!(
        who(cat.revoke_term(eu, "x").unwrap()),
        principals(&["ada", "bob"])
    );
    assert_eq!(
        who(cat.grant_permission(ada, Permission::Admin).unwrap()),
        principals(&["ada"])
    );
    assert_eq!(
        who(cat.grant_permission(ada, Permission::Admin).unwrap()),
        none
    );
    assert_eq!(
        who(cat.grant_permission(eu, Permission::Read).unwrap()),
        principals(&["ada", "bob"])
    );
    assert_eq!(
        who(cat.revoke_permission(eu, Permission::Read).unwrap()),
        principals(&["ada", "bob"])
    );
    assert_eq!(
        who(cat.set_bypass("cy", true).unwrap()),
        principals(&["cy"])
    );
    assert_eq!(who(cat.set_bypass("cy", true).unwrap()), none);
    assert_eq!(
        who(cat.disable_principal("cy").unwrap()),
        principals(&["cy"])
    );
    assert_eq!(who(cat.disable_principal("cy").unwrap()), none);
    assert_eq!(
        who(cat.enable_principal("cy").unwrap()),
        principals(&["cy"])
    );
}

#[test]
fn a_change_to_a_providers_rules_or_role_mappings_reports_the_provider() {
    let fx = Fixture::new();
    let cat = three_principals(&fx);
    let corp_only = providers(&["corp"]);
    assert_eq!(
        who(cat.create_provider(&corp(vec![groups_rule()])).unwrap()),
        Affected::default()
    );
    let rules = corp(vec![groups_rule(), tenant_rule()]);
    assert_eq!(who(cat.update_provider(&rules).unwrap()), corp_only);
    let mapped = Provider {
        role_mappings: vec![mapping("tessera-admins", "eu")],
        ..rules
    };
    assert_eq!(who(cat.update_provider(&mapped).unwrap()), corp_only);
    assert_eq!(who(cat.drop_provider("corp").unwrap()), corp_only);
}

#[test]
fn a_change_to_a_mapped_group_reports_its_members_and_every_provider_mapping_to_it() {
    let fx = Fixture::new();
    let cat = three_principals(&fx);
    cat.create_group("admins").unwrap();
    cat.add_member("admins", "cy").unwrap();
    cat.create_provider(&corp_with_admins()).unwrap();
    let partner = Provider {
        name: "partner".into(),
        role_mappings: vec![mapping("ops", "admins")],
        ..corp(vec![])
    };
    cat.create_provider(&partner).unwrap();
    cat.create_provider(&Provider {
        name: "other".into(),
        role_mappings: vec![mapping("tessera-admins", "eu")],
        ..corp(vec![])
    })
    .unwrap();
    let admins = Grantee::Group("admins");
    let reported = Affected {
        principals: set(&["cy"]),
        providers: set(&["corp", "partner"]),
        ..Affected::default()
    };
    assert_eq!(who(cat.grant_term(admins, "ops").unwrap()), reported);
    assert_eq!(who(cat.revoke_term(admins, "ops").unwrap()), reported);
    let admin = Permission::Admin;
    assert_eq!(who(cat.grant_permission(admins, admin).unwrap()), reported);
    assert_eq!(who(cat.revoke_permission(admins, admin).unwrap()), reported);
    assert_eq!(who(cat.delete_group("admins").unwrap()), reported);
    // A group no mapping names reports its members alone.
    cat.drop_provider("other").unwrap();
    let eu = Grantee::Group("eu");
    assert_eq!(
        who(cat.grant_permission(eu, Permission::Read).unwrap()),
        principals(&["ada", "bob"])
    );
}

#[test]
fn revoking_a_key_names_it_and_deleting_a_principal_names_it_and_its_keys() {
    let fx = Fixture::new();
    let cat = three_principals(&fx);
    let (k1, created) = cat.create_api_key("bob", None, None).unwrap();
    assert_eq!(who(created), Affected::default());
    let (k2, _) = cat.create_api_key("bob", None, None).unwrap();
    let (k3, _) = cat.create_api_key("ada", None, None).unwrap();
    assert_eq!(
        who(cat.revoke_api_key(&k3.prefix).unwrap()),
        Affected {
            api_keys: set(&[&k3.prefix]),
            ..Affected::default()
        }
    );
    assert_eq!(
        who(cat.delete_principal("bob").unwrap()),
        Affected {
            principals: set(&["bob"]),
            api_keys: set(&[&k1.prefix, &k2.prefix]),
            ..Affected::default()
        }
    );
}

#[test]
fn the_generation_rises_with_each_change_and_survives_reopening() {
    let fx = Fixture::new();
    let cat = fx.open();
    assert_eq!(cat.generation(), 0);
    assert_eq!(
        cat.create_principal("ada", PrincipalKind::Person)
            .unwrap()
            .generation,
        1
    );
    assert_eq!(
        cat.grant_term(Grantee::Principal("ada"), "x")
            .unwrap()
            .generation,
        2
    );
    // A change that alters nothing, and one that is refused, leave it as it was.
    assert_eq!(
        cat.grant_term(Grantee::Principal("ada"), "x")
            .unwrap()
            .generation,
        2
    );
    cat.grant_term(Grantee::Principal("bob"), "x").unwrap_err();
    assert_eq!(cat.generation(), 2);
    let (key, created) = cat.create_api_key("ada", None, None).unwrap();
    assert_eq!(created.generation, 3);
    assert_eq!(cat.resolve("ada", Some(&key.prefix)).unwrap().generation, 3);
    cat.create_provider(&corp(vec![groups_rule()])).unwrap();
    assert_eq!(
        cat.resolve_claims("corp", &json!({})).unwrap().generation,
        4
    );
    drop(cat);

    let cat = fx.open();
    assert_eq!(cat.generation(), 4);
    assert_eq!(cat.resolve("ada", None).unwrap().generation, 4);
    assert_eq!(cat.revoke_api_key(&key.prefix).unwrap().generation, 5);
}

/// Makes every write to every table in the file fail, as a full disc or a lost file would.
fn fail_every_write(conn: &Connection) {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for t in tables {
        for op in ["INSERT", "UPDATE", "DELETE"] {
            conn.execute_batch(&format!(
                "CREATE TRIGGER fail_{op}_{t} BEFORE {op} ON {t} \
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;"
            ))
            .unwrap();
        }
    }
}

#[test]
fn a_change_that_fails_to_commit_changes_nothing() {
    let fx = Fixture::new();
    let cat = fx.open();
    cat.create_principal("ada", PrincipalKind::Person).unwrap();
    cat.set_password("ada", "correct horse battery").unwrap();
    cat.create_group("eu").unwrap();
    cat.grant_term(Grantee::Group("eu"), "region:eu").unwrap();
    cat.create_provider(&corp(vec![groups_rule()])).unwrap();
    let (key, _) = cat.create_api_key("ada", None, None).unwrap();
    let snapshot = |cat: &Catalogue| {
        (
            cat.principals(),
            cat.groups(),
            cat.api_keys("ada"),
            cat.providers(),
            cat.resolve("ada", Some(&key.prefix)),
        )
    };
    let before = snapshot(&cat);

    let raw = fx.raw();
    fail_every_write(&raw);
    let ada = Grantee::Principal("ada");
    let attempts = [
        cat.create_principal("bob", PrincipalKind::Person),
        cat.disable_principal("ada"),
        cat.set_bypass("ada", true),
        cat.set_password("ada", "another long passphrase"),
        cat.clear_password("ada"),
        cat.create_group("ops"),
        cat.add_member("eu", "ada"),
        cat.grant_term(ada, "secret"),
        cat.revoke_term(Grantee::Group("eu"), "region:eu"),
        cat.grant_permission(ada, Permission::Admin),
        cat.revoke_api_key(&key.prefix),
        cat.create_api_key("ada", None, None).map(|(_, a)| a),
        cat.create_provider(&Provider {
            name: "partner".into(),
            ..corp(vec![])
        }),
        cat.update_provider(&corp_with_admins()),
        cat.drop_provider("corp"),
        cat.delete_group("eu"),
        cat.delete_principal("ada"),
    ];
    for (i, attempt) in attempts.into_iter().enumerate() {
        assert!(
            matches!(attempt, Err(Error::Storage(_))),
            "attempt {i}: {attempt:?}"
        );
    }
    assert_eq!(snapshot(&cat), before);
    assert!(cat.verify_password("ada", "correct horse battery").is_ok());
    assert!(cat.verify_api_key(&key.key).is_ok());

    let triggers: Vec<String> = raw
        .prepare("SELECT name FROM sqlite_master WHERE type = 'trigger'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for t in triggers {
        raw.execute_batch(&format!("DROP TRIGGER {t}")).unwrap();
    }
    drop(cat);
    assert_eq!(snapshot(&fx.open()), before);
}

#[test]
fn the_catalogue_is_shared_between_threads() {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<Catalogue>();
    let fx = Fixture::new();
    let cat = std::sync::Arc::new(fx.open());
    let workers: Vec<_> = (0..8)
        .map(|i| {
            let cat = std::sync::Arc::clone(&cat);
            std::thread::spawn(move || {
                let name = format!("p{i}");
                cat.create_principal(&name, PrincipalKind::Service).unwrap();
                cat.grant_term(Grantee::Principal(&name), &format!("t{i}"))
                    .unwrap();
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    drop(cat);
    let cat = fx.open();
    assert_eq!(cat.principals().len(), 8);
    for i in 0..8 {
        let r = cat.resolve(&format!("p{i}"), None).unwrap();
        assert_eq!(r.terms, set(&[&format!("t{i}")]));
    }
}

#[test]
fn many_terms_are_granted_and_revoked_as_one_change() {
    let fx = Fixture::new();
    let cat = fx.open();
    let ada = Grantee::Principal("ada");
    cat.create_principal("ada", PrincipalKind::Person).unwrap();
    let before = cat.generation();
    let granted = cat.grant_terms(ada, &["a", " b ", "c", "a"]).unwrap();
    assert_eq!(granted.generation, before + 1);
    assert_eq!(granted.principals, set(&["ada"]));
    // One refused term refuses them all, and nothing is granted.
    cat.grant_terms(ada, &["d", "public"]).unwrap_err();
    cat.revoke_terms(ada, &["a", "z"]).unwrap();
    drop(cat);

    let cat = fx.open();
    assert_eq!(cat.resolve("ada", None).unwrap().terms, set(&["b", "c"]));
    let unchanged = cat.grant_terms(ada, &["b", "c"]).unwrap();
    assert!(unchanged.is_empty());
}
