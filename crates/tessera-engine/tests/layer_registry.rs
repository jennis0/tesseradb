//! Stage 1's check: an empty layer exists, is reachable or not by gate, can be suppressed
//! immediately and dropped permanently — and all of it survives a restart.
//!
//! The four assertions here are the ones the artifact delivery plan names, and each fails a
//! different way when it is wrong. The reachability one is the disclosure assertion: a layer whose
//! gate a principal does not satisfy must be indistinguishable from a name nobody registered, so
//! the test asks about both and demands the same answer.

mod common;

use common::*;
use tessera_engine::Engine;
use tessera_lifecycle::wal::ChangeOp;
use tessera_types::layer::{
    ContentDeclaration, ExistenceCriterion, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource, ROWLESS_CEILING,
};

fn declaration(name: &str, visibility: Option<&str>) -> LayerDeclaration {
    LayerDeclaration {
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: visibility.map(str::to_string),
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: Some(ExistenceCriterion::Count(50)),
        hierarchy: Hierarchy {
            kind: HierarchyKind::Nested,
            prune_children: true,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into(), "hull".into()],
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// The names a principal holding `credential` may know about.
fn reachable(engine: &Engine, credential: &[u8]) -> Vec<String> {
    let session = engine.authorise(credential).unwrap();
    engine
        .visible_layers(&session)
        .into_iter()
        .map(|l| l.declaration.name)
        .collect()
}

/// Invert a layer's `tessera_id` the way `/control/changes` does — through the admin plane's own
/// resolver, so this exercises the misdirection guard rather than going round it.
///
/// **This is the assertion, not a convenience.** A layer's entity sits *above* the row-less mark,
/// and the guard used to be a single `entity < high_water` test, which refuses every layer
/// identifier this deployment has ever issued. The symptom would not have looked like a range
/// check: suppressing a layer would simply have answered *no such thing*.
fn layer_entity(engine: &Engine, id: tessera_types::TesseraId) -> tessera_types::EntityId {
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0]
        .expect("a layer identifier names the entity this deployment issued for it")
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture(
        &root,
        &tmp.path().join("points.parquet"),
        &tmp.path().join("pairs.parquet"),
    );
    Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        open_engine_publishing(&self.root, &self.cache, &self.wal)
    }
}

/// **The disclosure assertion.** A layer gated on a term the principal does not hold must be
/// exactly as absent as a layer nobody registered — and the registry answers both with one probe of
/// the same set, so there is no branch that could take a different amount of time.
#[test]
fn a_gate_failed_layer_is_indistinguishable_from_one_that_was_never_registered() {
    let fx = fixture();
    let engine = fx.open();

    // "0" is the term the full-coverage credential holds; "1" is the subset credential's.
    engine
        .register_layer(declaration("clusters/open", None))
        .expect("a `public` layer registers");
    engine
        .register_layer(declaration("clusters/restricted", Some("1")))
        .expect("a gated layer registers");

    let broad = reachable(&engine, &full_coverage_credential());
    assert!(broad.contains(&"clusters/open".to_string()));
    assert!(
        !broad.contains(&"clusters/restricted".to_string()),
        "the full-coverage credential holds term 0, not term 1"
    );
    assert!(
        !broad.contains(&"clusters/never-registered".to_string()),
        "and a name nobody registered is absent by the same route"
    );
    // The two absences are the same absence: neither name is in the resolved set at all, so a
    // caller cannot tell "you may not see this" from "this does not exist".
    assert_eq!(broad, vec!["clusters/open".to_string()]);

    // The principal who does hold the term sees it, which is what makes the absence above a gate
    // rather than a bug.
    let narrow = reachable(&engine, &subset_credential());
    assert!(narrow.contains(&"clusters/restricted".to_string()));
}

/// A layer takes an entity precisely so that `/control/changes` works on it unchanged. The
/// suppression is live at the ack — no flush, no republish, no new session.
#[test]
fn suppressing_a_layers_entity_removes_it_at_the_ack() {
    let fx = fixture();
    let engine = fx.open();

    let id = engine
        .register_layer(declaration("clusters/a", None))
        .expect("registers");
    assert_eq!(reachable(&engine, &full_coverage_credential()).len(), 1);

    let entity = layer_entity(&engine, id);
    engine
        .accept_change(entity, ChangeOp::Suppress)
        .expect("a layer suppression is an ordinary change");

    assert!(
        reachable(&engine, &full_coverage_credential()).is_empty(),
        "suppression takes effect at the ack, with nothing republished"
    );

    // And it comes back, because a suppression is the reversible one of the two removal rules.
    engine.accept_change(entity, ChangeOp::Unsuppress).unwrap();
    assert_eq!(reachable(&engine, &full_coverage_credential()).len(), 1);
}

/// A dropped name is refused for ever. Bookmarks, edges and suppressions travel by name, so a
/// recreated layer would silently inherit every stale reference to the old one.
#[test]
fn a_dropped_name_is_refused_on_recreation_and_stays_refused_across_a_restart() {
    let fx = fixture();
    {
        let engine = fx.open();
        engine
            .register_layer(declaration("clusters/a", None))
            .unwrap();
        engine.drop_layer("clusters/a".into()).unwrap();
        assert!(reachable(&engine, &full_coverage_credential()).is_empty());

        let refused = engine
            .register_layer(declaration("clusters/a", None))
            .expect_err("the name is tombstoned");
        assert!(
            refused.to_string().contains("dropped"),
            "the refusal says why, since it is the caller's to fix: {refused}"
        );
    }

    // The tombstone is durable. A restart that forgot it would hand the name back — which is the
    // whole failure the tombstone exists to prevent, arriving through recovery rather than through
    // the registry.
    let engine = fx.open();
    assert!(engine
        .register_layer(declaration("clusters/a", None))
        .is_err());
}

/// The registry, its gate, and the entity that carries its suppression all survive a restart —
/// through the WAL, before any flush has published a manifest section.
#[test]
fn a_registration_and_its_suppression_survive_a_restart_through_the_wal() {
    let fx = fixture();
    let (id, low_water) = {
        let engine = fx.open();
        let id = engine
            .register_layer(declaration("clusters/a", None))
            .unwrap();
        engine
            .register_layer(declaration("clusters/gated", Some("1")))
            .unwrap();
        let entity = layer_entity(&engine, id);
        engine.accept_change(entity, ChangeOp::Suppress).unwrap();
        (id, engine.allocator_low_water())
    };

    let engine = fx.open();
    // The gate replayed with the declaration: `clusters/gated` is still gated, not reachable.
    assert_eq!(
        reachable(&engine, &full_coverage_credential()),
        Vec::<String>::new(),
        "one layer is suppressed and the other is gated; neither reaches this principal"
    );
    assert_eq!(
        reachable(&engine, &subset_credential()),
        vec!["clusters/gated".to_string()],
        "and the gated one still reaches the principal who holds its term"
    );

    // The identifier is stable across the restart, which is what makes a held bookmark work — and
    // the mark did not move back up, which is what stops the next registration reissuing these ids.
    let entity = layer_entity(&engine, id);
    engine.accept_change(entity, ChangeOp::Unsuppress).unwrap();
    assert_eq!(
        reachable(&engine, &full_coverage_credential()),
        vec!["clusters/a".to_string()]
    );
    assert_eq!(engine.allocator_low_water(), low_water);
}

/// Row-less allocation must not touch the point region, and both marks must come back from a
/// restart. A mark that reset to the ceiling would reissue a live layer's entity to a point: two
/// entities, one `tessera_id`.
#[test]
fn the_two_regions_stay_apart_and_both_marks_survive() {
    let fx = fixture();
    let (high, low) = {
        let engine = fx.open();
        let before = engine.allocator_high_water();
        for name in ["a", "b", "c"] {
            engine
                .register_layer(declaration(&format!("clusters/{name}"), None))
                .unwrap();
        }
        assert_eq!(
            engine.allocator_high_water(),
            before,
            "registering a layer allocates no point ids"
        );
        assert!(
            engine.allocator_low_water() < ROWLESS_CEILING,
            "and it did allocate row-less ones"
        );
        (engine.allocator_high_water(), engine.allocator_low_water())
    };

    let engine = fx.open();
    assert_eq!(engine.allocator_high_water(), high);
    assert_eq!(
        engine.allocator_low_water(),
        low,
        "the row-less mark is recovered, not reset — resetting reissues a live layer's ids"
    );
}

/// A declaration that contradicts itself is refused before anything is allocated or appended, and
/// the message says what to fix.
#[test]
fn an_incoherent_declaration_is_refused_with_nothing_spent() {
    let fx = fixture();
    let engine = fx.open();
    let before = engine.allocator_low_water();

    // Decision 0082's one forbidden combination: a tree's lineage is its edges, so it declares no
    // levels.
    let mut treed_with_levels = declaration("clusters/bad", None);
    treed_with_levels.levels = vec![tessera_types::layer::LevelDeclaration {
        level: 0,
        title: Some("L0".into()),
        zoom: None,
    }];
    let refused = engine
        .register_layer(treed_with_levels)
        .expect_err("a nested layer may not declare levels");
    assert!(refused.to_string().contains("edges"), "{refused}");

    assert_eq!(
        engine.allocator_low_water(),
        before,
        "a refusal spends no ids: every check runs before the first allocation"
    );
    assert!(reachable(&engine, &full_coverage_credential()).is_empty());
}
