//! **Stage 3's first half: the shape beside a cluster is the viewer's own, and never the
//! cluster's.**
//!
//! A masked count is obviously a per-viewer quantity, and a centroid is not — it *looks* like a
//! property of the cluster, which is exactly why serving a build-time one is the fail-open the
//! closure rule exists to close (`annotations.md` §4.2): a derived property is a function of
//! `membership ∩ M_auth` and of nothing else. Every expectation here is computed from the fixture's
//! own generator arithmetic rather than from anything the engine said, because an assertion against
//! the engine's own answer passes whatever the engine does.

mod common;

#[path = "common/ring.rs"]
mod ring;

use common::*;
use tessera_engine::derived::DerivedContent;
use tessera_engine::{ArtifactOut, Engine, ViewportRequest};
use tessera_lifecycle::membership::IncomingContent;
use tessera_lifecycle::IncomingArtifact;
use tessera_spatial::morton::fixed32;
use tessera_types::layer::{
    ContentDeclaration, DeclarationError, Hierarchy, HierarchyKind, LayerDeclaration,
    MembershipSource,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

fn declaration(name: &str, derived: &[&str]) -> LayerDeclaration {
    LayerDeclaration {
        scope: Default::default(),
        name: name.into(),
        title: Some(format!("{name} (title)")),
        views: vec!["s0".into()],
        membership: MembershipSource::Enumerated,
        value_set: Default::default(),
        visibility: None,
        artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
        require_member_visibility: None,
        hierarchy: Hierarchy {
            kind: HierarchyKind::Flat,
            prune_children: false,
        },
        content: ContentDeclaration {
            computed: derived.iter().map(|d| (*d).to_string()).collect(),
            supplied: Vec::new(),
            withdraw_on_member_deletion: true,
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
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

    fn members(&self, source_ids: impl Iterator<Item = u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }
}

/// The generator's own position for a source id, in the grid units the wire carries — `x = 37 s mod
/// 1000` and `y = 53 s mod 1000` (`common::write_points_n`), quantised against the fixture's
/// extent.
///
/// **Computed here rather than read back from the engine**, so the expectations below are
/// independent of the thing under test.
fn grid_position(source_id: u64) -> [u32; 2] {
    let e = extent();
    [
        fixed32(((source_id * 37) % 1000) as f64, e.x_min, e.x_max),
        fixed32(((source_id * 53) % 1000) as f64, e.y_min, e.y_max),
    ]
}

/// The mean position of these source ids, in grid units.
fn expected_centroid(source_ids: impl Iterator<Item = u64>) -> [f64; 2] {
    let positions: Vec<[u32; 2]> = source_ids.map(grid_position).collect();
    let n = positions.len() as f64;
    [
        positions.iter().map(|p| p[0] as f64).sum::<f64>() / n,
        positions.iter().map(|p| p[1] as f64).sum::<f64>() / n,
    ]
}

fn visible_to_subset(source_ids: impl Iterator<Item = u64>) -> Vec<u64> {
    source_ids
        .filter(|s| terms_of(*s).contains(&SUBSET_TERM))
        .collect()
}

fn artifacts_of(engine: &Engine, credential: &[u8]) -> Vec<ArtifactOut> {
    let session = engine.authorise(credential).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

fn publish(engine: &Engine, layer: &str, fx: &Fixture, sources: impl Iterator<Item = u64>) {
    engine
        .publish_artifacts(
            layer.into(),
            0,
            vec![IncomingArtifact::from_entities(
                Some("c0".into()),
                fx.members(sources),
            )],
        )
        .unwrap();
}

/// Two principals close enough for a rounding comparison: the centroid is a mean of up to 2^32-wide
/// coordinates, so an exact float equality would be asserting the summation order rather than the
/// answer. One grid unit is 1/2^32 of the extent — far below a pixel at any zoom.
fn assert_close(got: [f64; 2], want: [f64; 2], what: &str) {
    for axis in 0..2 {
        assert!(
            (got[axis] - want[axis]).abs() < 1.0,
            "{what}: axis {axis} was {} and should be {}",
            got[axis],
            want[axis]
        );
    }
}

/// **The stage's headline for derived content.** One cluster, two principals, two centroids — and
/// the narrow one is the mean of the members *that principal* can see, not the cluster's own mean.
///
/// A centroid over full membership would be identical for both, which is what makes this the
/// dangerous one: it looks exactly like the number the engine is supposed to produce, and it
/// describes documents the narrow viewer may not see.
#[test]
fn the_centroid_is_the_viewers_own_and_never_the_artifacts() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/a", &["centroid"]))
        .unwrap();
    let sources: Vec<u64> = (0..300).collect();
    publish(&engine, "clusters/a", &fx, sources.iter().copied());

    let broad = artifacts_of(&engine, &full_coverage_credential());
    let narrow = artifacts_of(&engine, &subset_credential());
    assert_eq!(broad.len(), 1);
    assert_eq!(narrow.len(), 1);

    let whole = expected_centroid(sources.iter().copied());
    let visible = visible_to_subset(sources.iter().copied());
    assert_close(
        broad[0].derived.centroid.expect("declared"),
        whole,
        "the broad principal sees every member, so their centroid is the whole cluster's",
    );
    assert_close(
        narrow[0].derived.centroid.expect("declared"),
        expected_centroid(visible.iter().copied()),
        "the narrow principal's centroid is over the members they can see",
    );
    // And the two are genuinely different: a build-time centroid would make this assertion fail
    // while every other assertion in this file still passed.
    let n = narrow[0].derived.centroid.unwrap();
    assert!(
        (n[0] - whole[0]).abs() > 1.0 || (n[1] - whole[1]).abs() > 1.0,
        "the two principals' centroids must not coincide, or this fixture proves nothing"
    );
}

/// The box and the hull follow the same rule, and the hull's vertices are positions of members this
/// principal can see — never of one they cannot.
#[test]
fn the_box_and_the_hull_are_drawn_from_visible_members_alone() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/a", &["box", "hull"]))
        .unwrap();
    let sources: Vec<u64> = (0..300).collect();
    publish(&engine, "clusters/a", &fx, sources.iter().copied());

    let narrow = artifacts_of(&engine, &subset_credential());
    let bbox = narrow[0].derived.bbox.expect("declared");
    let hull = narrow[0].derived.shape.clone().expect("declared");

    let visible: Vec<[u32; 2]> = visible_to_subset(sources.iter().copied())
        .into_iter()
        .map(grid_position)
        .collect();
    let want = [
        visible.iter().map(|p| p[0]).min().unwrap(),
        visible.iter().map(|p| p[1]).min().unwrap(),
        visible.iter().map(|p| p[0]).max().unwrap(),
        visible.iter().map(|p| p[1]).max().unwrap(),
    ];
    assert_eq!(bbox, want, "the box bounds the visible members exactly");

    // Every hull vertex, in every ring, is a visible member's position. A vertex that is not is a
    // position this principal was never entitled to, arriving as geometry.
    for vertex in hull.iter().flatten().flatten() {
        assert!(
            visible.contains(vertex),
            "{vertex:?} is not the position of any member this principal can see"
        );
    }
    // A hull with a vertex outside the box would be incoherent; a hull *inside* the box's corners
    // is ordinary, since the corners need not be occupied.
    for vertex in hull.iter().flatten().flatten() {
        assert!(vertex[0] >= want[0] && vertex[0] <= want[2]);
        assert!(vertex[1] >= want[1] && vertex[1] <= want[3]);
    }

    // The broad principal's box strictly contains the narrow one's, because they see strictly more
    // members. Equal boxes would mean the mask was not applied.
    let broad = artifacts_of(&engine, &full_coverage_credential());
    let broad_box = broad[0].derived.bbox.expect("declared");
    assert!(
        broad_box[0] <= want[0]
            && broad_box[1] <= want[1]
            && broad_box[2] >= want[2]
            && broad_box[3] >= want[3],
        "the broad box {broad_box:?} must contain the narrow one {want:?}"
    );
    assert_ne!(broad_box, want, "and must not be the same box");
}

/// **The served hull is a concave shape and not the convex wrap it replaced.** It is tighter than
/// the wrap over the very same visible members, and it still holds every one of them — the two
/// halves of what a hull is for. Both are checked against a wrap computed here, from the fixture's
/// own generator arithmetic, so neither rests on the engine agreeing with itself.
#[test]
fn the_served_hull_is_tighter_than_its_wrap_and_holds_every_visible_member() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/a", &["hull"]))
        .unwrap();
    let sources: Vec<u64> = (0..300).collect();
    publish(&engine, "clusters/a", &fx, sources.iter().copied());

    for credential in [subset_credential(), full_coverage_credential()] {
        let served = artifacts_of(&engine, &credential);
        let hull = served[0].derived.shape.clone().expect("declared");
        let visible: Vec<[u32; 2]> = if credential == subset_credential() {
            visible_to_subset(sources.iter().copied())
                .into_iter()
                .map(grid_position)
                .collect()
        } else {
            sources.iter().copied().map(grid_position).collect()
        };

        for member in &visible {
            assert!(
                hull.iter().flatten().any(|r| ring::contains(r, *member)),
                "{member:?} is a member this principal sees and it fell outside every ring of its hull"
            );
        }
        let wrap = ring::convex_hull(&visible);
        assert!(
            hull.iter()
                .flatten()
                .map(|r| ring::double_area(r))
                .sum::<i128>()
                < ring::double_area(&wrap),
            "the served hull is the convex wrap, not a shape that follows the members"
        );
    }
}

/// A layer that declares nothing gets nothing — and pays nothing. The count is intrinsic and is
/// still there.
#[test]
fn a_layer_declaring_no_derived_content_serves_none() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/a", &[]))
        .unwrap();
    publish(&engine, "clusters/a", &fx, 0..300);

    let served = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(served[0].derived, DerivedContent::default());
    assert!(served[0].masked_count > 0, "the count is intrinsic");
}

/// Only what the layer declared. A client draws what `/v1/meta` says the layer carries, so an
/// undeclared property arriving anyway would be content nobody asked for and nobody validated.
#[test]
fn only_the_declared_properties_are_computed() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/a", &["centroid"]))
        .unwrap();
    publish(&engine, "clusters/a", &fx, 0..300);

    let served = artifacts_of(&engine, &full_coverage_credential());
    assert!(served[0].derived.centroid.is_some());
    assert!(served[0].derived.bbox.is_none(), "not declared");
    assert!(served[0].derived.shape.is_none(), "not declared");
}

/// **The drill-down computes the same geometry as the viewport**, because both call the same code
/// against the same composed mask. Two transcriptions of one rule is the failure this test exists
/// to catch, and it is the same argument that put the predicate itself in one place.
#[test]
fn the_drill_down_agrees_with_the_viewport_on_derived_content() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/a", &["centroid", "box", "hull"]))
        .unwrap();
    publish(&engine, "clusters/a", &fx, 0..300);

    let session = engine.authorise(&subset_credential()).unwrap();
    let from_viewport = artifacts_of(&engine, &subset_credential());
    let idset = engine.generation().bundle.manifest.identity.idset;
    let drilled = engine
        .artifact(
            &session,
            from_viewport[0].tessera_id,
            Some(idset),
            "s0",
            None,
        )
        .unwrap()
        .expect("the identifier the viewport just issued");

    assert_eq!(drilled.derived, from_viewport[0].derived);
    assert_eq!(drilled.masked_count, from_viewport[0].masked_count);
}

/// A property the engine does not compute is **refused at registration**, not accepted and quietly
/// omitted. A served artifact missing content its layer declared cannot be told apart, by a client,
/// from one whose content was withheld — and nothing is ever withheld from a served artifact.
#[test]
fn a_layer_declaring_a_property_the_engine_does_not_compute_is_refused() {
    let fx = fixture();
    let engine = fx.open();

    let err = engine
        .register_layer(declaration("clusters/a", &["extractive_terms"]))
        .expect_err("⊘ specified and not implemented, so refused rather than silently absent");
    assert!(
        format!("{err}").contains("extractive_terms"),
        "the refusal names what was declared: {err}"
    );

    // And the refusal is fail-closed: the layer does not exist afterwards, so a principal who
    // reaches everything still reaches nothing by that name.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(engine
        .visible_layers(&session)
        .iter()
        .all(|l| l.declaration.name != "clusters/a"));
    // The declaration itself refuses before any engine state is touched.
    assert!(matches!(
        declaration("clusters/a", &["hulls"]).validate(),
        Err(DeclarationError::UnknownComputed(name)) if name == "hulls"
    ));
}

// ---- supplied content, and the test that decides who may read it ----------------------------

fn label_layer(name: &str, requires_all_members: bool) -> LayerDeclaration {
    let mut d = declaration(name, &[]);
    d.content.supplied = vec![tessera_types::layer::SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: if requires_all_members {
            tessera_types::layer::SuppliedRequirement::All
        } else {
            tessera_types::layer::SuppliedRequirement::Inherited
        },
    }];
    d
}

fn content(text: &str, fx: &Fixture, generated_from: impl Iterator<Item = u64>) -> IncomingContent {
    IncomingContent::new(vec![text.to_string()], fx.members(generated_from))
}

/// **The stage's headline.** A label is served only to a viewer who can see every document it was
/// generated from — and the viewer who fails here sees a great deal of the corpus, because what
/// decides is which documents and never how many.
#[test]
fn a_label_is_served_only_to_a_viewer_who_can_see_everything_behind_it() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(label_layer("topics/a", true))
        .unwrap();

    // Generated from a sample holding documents the narrow principal cannot see: the fixture gives
    // term 1 to every third source id, so 0..30 holds twenty it cannot.
    engine
        .publish_artifacts(
            "topics/a".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                fx.members(0..300),
                vec![content("a label from the whole sample", &fx, 0..30)],
            )],
        )
        .unwrap();

    let broad = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(broad.len(), 1);
    assert_eq!(broad[0].content, vec!["a label from the whole sample"]);

    let narrow = artifacts_of(&engine, &subset_credential());
    assert!(
        narrow.is_empty(),
        "the narrow principal sees a third of the generating set, so they receive no artifact — \
         not the artifact with its label missing: {narrow:?}"
    );
}

/// Ranked contents: both principals fail the same full-sample label, and both are served the
/// narrower one. The design's own worked example.
#[test]
fn both_principals_fail_the_same_label_and_both_satisfy_its_narrower_variant() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(label_layer("topics/a", true))
        .unwrap();

    let narrow_sample: Vec<u64> = (0..30)
        .filter(|s| terms_of(*s).contains(&SUBSET_TERM))
        .collect();
    engine
        .publish_artifacts(
            "topics/a".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t0".into()),
                fx.members(0..300),
                vec![
                    content("the whole sample", &fx, 0..300),
                    content("one term's worth", &fx, narrow_sample.iter().copied()),
                ],
            )],
        )
        .unwrap();

    let narrow = artifacts_of(&engine, &subset_credential());
    assert_eq!(narrow.len(), 1);
    assert_eq!(
        narrow[0].content,
        vec!["one term's worth"],
        "the first content this principal contains entirely — never the ranked-first one they \
         do not"
    );
    // A principal holding nothing at all is served neither, and no artifact.
    assert!(artifacts_of(&engine, &zero_credential()).is_empty());
}

/// Corpus-independent content — an authored name — has an empty generating set, so containment is
/// vacuous and everyone who reaches the layer reads it.
#[test]
fn corpus_independent_content_is_served_to_everyone_who_reaches_the_layer() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(label_layer("programmes/a", false))
        .unwrap();
    engine
        .publish_artifacts(
            "programmes/a".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("p0".into()),
                fx.members(0..300),
                vec![IncomingContent::new(vec!["An authored name".into()], [])],
            )],
        )
        .unwrap();

    for credential in [full_coverage_credential(), subset_credential()] {
        let served = artifacts_of(&engine, &credential);
        assert_eq!(served.len(), 1);
        assert_eq!(served[0].content, vec!["An authored name"]);
    }
}

/// Delete every member of the WAL sequence — the log is a sequence beside the configured base
/// path, and the base path itself is never a file.
fn remove_the_whole_log(fx: &Fixture) {
    let dir = fx.wal.parent().expect("the log has a directory");
    let stem = fx.wal.file_stem().expect("the log has a stem").to_owned();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
    {
        let name = entry.file_name();
        if name
            .to_string_lossy()
            .starts_with(&format!("{}-", stem.to_string_lossy()))
        {
            std::fs::remove_file(entry.path()).expect("a log member is removable");
            removed += 1;
        }
    }
    assert!(
        removed > 0,
        "no log member was found to delete — the test would prove nothing"
    );
}

/// Wait for the executor's drain close to publish at least `want` artifact content extents.
fn content_extents(fx: &Fixture, want: usize) -> Vec<std::path::PathBuf> {
    let dir = fx
        .root
        .join("v00000")
        .join("partitions")
        .join("default")
        .join("attrs")
        .join("record")
        .join("extents");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let found: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("artifacts-"))
                    && p.to_string_lossy().ends_with(".blocks.bin")
            })
            .collect();
        if found.len() >= want {
            return found;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {} of {want} content extents were published within 10s",
            found.len()
        );
        std::thread::yield_now();
    }
}

/// **Two publications, and both labels survive the loss of the log.**
///
/// The regression this exists for: an artifact publication starts from a *clone of the stale
/// generation's* manifest, so a second publication that merely appended its extent to that clone
/// drops the first one's entry. The file stays on disk, named by nothing — and once the log is
/// released, the text in it is the only copy. The artifact then comes back with content that
/// cannot be read and is withheld from every viewer, silently, which is indistinguishable from a
/// containment failure.
///
/// A single publication would pass whatever the manifest did, which is why this publishes twice.
#[test]
fn two_publications_of_content_both_survive_the_loss_of_the_whole_log() {
    let fx = fixture();
    {
        let engine = fx.open();
        engine
            .register_layer(label_layer("topics/a", true))
            .unwrap();
        engine
            .publish_artifacts(
                "topics/a".into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some("t0".into()),
                    fx.members(0..100),
                    vec![content("the first label", &fx, 0..10)],
                )],
            )
            .unwrap();
        content_extents(&fx, 1);

        engine
            .publish_artifacts(
                "topics/a".into(),
                0,
                vec![IncomingArtifact::with_content(
                    Some("t1".into()),
                    fx.members(100..200),
                    vec![content("the second label", &fx, 100..110)],
                )],
            )
            .unwrap();
        content_extents(&fx, 2);
    }

    remove_the_whole_log(&fx);
    let engine = fx.open();
    let served = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(
        served.len(),
        2,
        "both artifacts came back from the manifest, with no log to replay: {served:?}"
    );
    let labels: Vec<&str> = served
        .iter()
        .map(|a| a.content.first().expect("content survived").as_str())
        .collect();
    assert_eq!(
        labels,
        vec!["the first label", "the second label"],
        "each artifact kept its own text — the first publication's extent must still be named by \
         the manifest the second one wrote"
    );
}

/// The four ways a batch can disagree with what its layer declared, each refused at publication —
/// and each refused by **its own** rule, which is why the assertions read the message rather than
/// the `is_err()` beneath it.
///
/// Three of the four go to `topics/a`, a layer no successful publication here exercises. A bare
/// `is_err()` on those is satisfied by any refusal at all — a layer that failed to register usably
/// would pass three of them, and the closing emptiness check is consistent with that too.
///
/// **Mutations this kills:** a rule silently subsumed by an earlier check, or two of the four
/// collapsed onto one refusal path — any change that leaves the batch refused for the wrong reason.
/// The fourth case is the one to watch: an empty generating set on corpus-derived content is what
/// keeps `a_permissive_layer_shrinks_the_generating_set_at_the_fold_and_serves_again`'s
/// empty-set condition out at the front door, and its identity was pinned by nothing.
#[test]
fn content_that_disagrees_with_the_declaration_is_refused() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(label_layer("topics/a", true))
        .unwrap();
    engine
        .register_layer(declaration("clusters/plain", &[]))
        .unwrap();

    // The refusal a batch draws, as an operator would read it. An accepted batch is itself the
    // failure: a declaration rule that admits its own violation.
    let refusal = |layer: &str, rule: &str, artifact: IncomingArtifact| -> String {
        match engine.publish_artifacts(layer.into(), 0, vec![artifact]) {
            Ok(_) => panic!("{rule}: the batch was published rather than refused"),
            Err(error) => error.to_string(),
        }
    };

    // Content on a layer that declares none.
    let undeclared = refusal(
        "clusters/plain",
        "content under no declared kind",
        IncomingArtifact::with_content(
            Some("c0".into()),
            fx.members(0..10),
            vec![content("a label", &fx, 0..10)],
        ),
    );
    assert!(
        undeclared.contains("carries supplied content, and this layer declares none"),
        "content on a layer declaring none must be refused by that rule and not by another: \
         {undeclared}"
    );

    // No content on a layer that declares some.
    let missing = refusal(
        "topics/a",
        "a declared kind left unsupplied",
        IncomingArtifact::from_entities(Some("t1".into()), fx.members(0..10)),
    );
    assert!(
        missing.contains("carries no supplied content, and this layer declares 1 kind(s)"),
        "an artifact short of a kind its layer declares must be refused by that rule, naming the \
         count it fell short of: {missing}"
    );

    // A content supplying the wrong number of values.
    let arity = refusal(
        "topics/a",
        "a content of the wrong arity",
        IncomingArtifact::with_content(
            Some("t2".into()),
            fx.members(0..10),
            vec![IncomingContent::new(
                vec!["a".into(), "b".into()],
                fx.members(0..10),
            )],
        ),
    );
    assert!(
        arity.contains("supplies 2 value(s) for 1 declared kind(s)"),
        "a content that is not a whole description must be refused on its arity, naming both \
         sides of it: {arity}"
    );

    // No generating set on corpus-derived content — the one that would otherwise serve to everyone.
    let vacuous = refusal(
        "topics/a",
        "corpus-derived content with no generating set",
        IncomingArtifact::with_content(
            Some("t3".into()),
            fx.members(0..10),
            vec![IncomingContent::new(vec!["a label".into()], [])],
        ),
    );
    assert!(
        vacuous.contains("contents[0] declares no generating set"),
        "an empty generating set under a requirement that every member be visible must be refused \
         as such — the refusal that keeps a set satisfied by everyone off a corpus-derived layer: \
         {vacuous}"
    );

    // Four rules, four refusals: a change collapsing any two onto one path moves this.
    let distinct: std::collections::BTreeSet<&String> = [&undeclared, &missing, &arity, &vacuous]
        .into_iter()
        .collect();
    assert_eq!(
        distinct.len(),
        4,
        "each rule refuses in its own words, so an operator can tell which one they broke"
    );

    // Every batch was refused whole, so nothing landed under any of those keys.
    assert!(artifacts_of(&engine, &full_coverage_credential()).is_empty());
}

/// Publish `labels.len()` artifacts, each with its own key and its own text, wait for the content
/// to reach a record extent, and then take the log away — so every later read of these labels comes
/// from the blob rather than from the publication's own copy in the write store.
///
/// The keys are `t0…`, and the *n*th artifact's members are the *n*th hundred of source ids, which
/// keeps each generating set inside what the full-coverage principal can see.
fn published_and_log_free(fx: &Fixture, labels: &[&str]) {
    {
        let engine = fx.open();
        engine
            .register_layer(label_layer("topics/a", true))
            .unwrap();
        let batch: Vec<IncomingArtifact> = labels
            .iter()
            .enumerate()
            .map(|(n, label)| {
                let members = (n as u64) * 20..(n as u64 + 1) * 20;
                IncomingArtifact::with_content(
                    Some(format!("t{n}")),
                    fx.members(members.clone()),
                    vec![content(label, fx, members)],
                )
            })
            .collect();
        engine
            .publish_artifacts("topics/a".into(), 0, batch)
            .unwrap();
        content_extents(fx, 1);
    }
    remove_the_whole_log(fx);
}

/// **Every artifact keeps its own text when the level is read a level at a time.**
///
/// The viewport reads a level's supplied content in one pass over the record blob and answers each
/// served artifact from it (`crate::artifact_content`), where it once read a zstd block per
/// artifact. The failure that pass makes possible is an addressing one — a table keyed a row out
/// would serve every artifact its neighbour's name, which is a wrong answer that looks entirely
/// well-formed — so what is asserted is the *pairing* of key to text, artifact by artifact, and not
/// merely that text arrived.
#[test]
fn every_artifact_keeps_its_own_content_when_the_level_is_read_from_the_blob() {
    let fx = fixture();
    let labels = [
        "the first label",
        "the second label",
        "the third label",
        "the fourth label",
        "the fifth label",
        "the sixth label",
    ];
    published_and_log_free(&fx, &labels);

    let engine = fx.open();
    let served = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(served.len(), labels.len(), "{served:?}");
    for artifact in &served {
        let key = artifact.key.as_deref().expect("each artifact kept its key");
        let n: usize = key.trim_start_matches('t').parse().expect("a t<n> key");
        assert_eq!(
            artifact.content,
            vec![labels[n].to_string()],
            "{key} was served its own text"
        );
    }

    // The drill-down reads the one entity's row directly rather than the level's table, and the
    // two routes must serve one string: a table that disagreed with the row it was built from
    // would show up here and nowhere else.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let idset = engine.generation().bundle.manifest.identity.idset;
    for artifact in &served {
        let drilled = engine
            .artifact(&session, artifact.tessera_id, Some(idset), "s0", None)
            .unwrap()
            .expect("the identifier the viewport just issued");
        assert_eq!(drilled.content, artifact.content);
    }
}

/// **A publication after the table is built is reflected in the next response**, because the level's
/// version is half the key it is held under.
///
/// The counter is what is asserted, not just the content: a table rebuilt on every request would
/// serve the right names and lose the whole point of holding one, and a table that outlived its
/// level would serve the level as it was — names that are all still names, which no assertion on
/// the text alone would catch.
#[test]
fn a_publication_after_the_table_is_built_rebuilds_it() {
    let fx = fixture();
    published_and_log_free(&fx, &["the first label", "the second label"]);

    let engine = fx.open();
    assert_eq!(engine.artifact_content_cache_stats().builds, 0);

    let first = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(first.len(), 2);
    let stats = engine.artifact_content_cache_stats();
    assert_eq!(stats.builds, 1, "the level's contents were read once");
    assert_eq!(stats.held, 1, "one table, for the one level served");
    assert_eq!(stats.artifacts, 2);

    // A second request at the same level version reads nothing.
    assert_eq!(artifacts_of(&engine, &full_coverage_credential()).len(), 2);
    assert_eq!(engine.artifact_content_cache_stats().builds, 1);

    engine
        .publish_artifacts(
            "topics/a".into(),
            0,
            vec![IncomingArtifact::with_content(
                Some("t2".into()),
                fx.members(200..220),
                vec![content("the third label", &fx, 200..220)],
            )],
        )
        .unwrap();

    let after = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(after.len(), 3);
    let mut labels: Vec<&str> = after
        .iter()
        .map(|a| a.content.first().expect("content survived").as_str())
        .collect();
    labels.sort_unstable();
    assert_eq!(
        labels,
        vec!["the first label", "the second label", "the third label"],
        "the two blob-resident labels came back beside the one still held in the write store"
    );
    assert_eq!(
        engine.artifact_content_cache_stats().builds,
        2,
        "the publication moved the level's version, so the table was read again"
    );
}
