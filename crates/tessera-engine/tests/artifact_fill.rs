//! **A fixed part of an artifact is filled once, through the record that carries it**
//! (`ingest.md` §1.5; decision 0136 R3, R4, R5).
//!
//! A `PUT` naming a key the level holds is accepted under the fill rule and mints nothing; a
//! `PATCH` fills a parent, an attachment, a content or a shape the artifact lacks; a differing
//! part is refused naming the part and never the value. Each case here ends where the write
//! path's failures are quiet: a restart, and for a filled content the reclaimed log, since the
//! fold carries content extents forward and the values reach one only through the tail pack.
//!
//! The lineage cache is keyed on the level's own lineage version (`ingest.md` §4.1): a page of
//! members joining leaves the held hierarchy alone, and a parent fill is what moves it.

mod common;

use common::*;
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::{ArtifactOut, Engine};
use tessera_lifecycle::membership::IncomingContent;
use tessera_lifecycle::{IncomingArtifact, IncomingGrowth};
use tessera_types::layer::{
    ContentDeclaration, Hierarchy, HierarchyKind, LayerDeclaration, MembershipSource,
    SuppliedContent, SuppliedRequirement,
};
use tessera_types::EntityId;

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// **No existence criterion**, so a count that moved is a membership that moved and an artifact
/// that vanished is one withheld — the two things these cases distinguish.
fn declaration(name: &str, kind: HierarchyKind) -> LayerDeclaration {
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
            kind,
            prune_children: true,
        },
        content: ContentDeclaration {
            computed: vec!["centroid".into()],
            supplied: Vec::new(),
        },
        depends_on: Vec::new(),
        levels: Vec::new(),
        layout: None,
        shape: None,
    }
}

/// A layer declaring one supplied content that needs no generating set, so a content can be
/// filled on its own.
fn described(name: &str) -> LayerDeclaration {
    let mut d = declaration(name, HierarchyKind::Flat);
    d.content.supplied = vec![SuppliedContent {
        name: "topic".into(),
        ty: "text".into(),
        require_member_visibility: SuppliedRequirement::Inherited,
    }];
    d
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
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    fn members(&self, source_ids: std::ops::Range<u64>) -> Vec<EntityId> {
        let map = source_to_new_map(&self.root, "v00000");
        source_ids.map(|s| EntityId::new(map[&s])).collect()
    }
}

fn node(
    fx: &Fixture,
    key: &str,
    parent: Option<&str>,
    sources: std::ops::Range<u64>,
) -> IncomingArtifact {
    let mut artifact = IncomingArtifact::from_entities(Some(key.into()), fx.members(sources));
    artifact.parent_keys = parent.into_iter().map(str::to_string).collect();
    artifact
}

fn artifacts_of(engine: &Engine) -> Vec<ArtifactOut> {
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 0, WHOLE_MAP, N_ITEMS as usize),
        )
        .expect("a viewport over the whole map")
        .artifacts
}

/// One layer's served keys with their masked counts, sorted.
fn served(engine: &Engine, layer: &str) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = artifacts_of(engine)
        .into_iter()
        .filter(|a| a.layer == layer)
        .filter_map(|a| a.key.clone().map(|key| (key, a.masked_count)))
        .collect();
    out.sort();
    out
}

/// The served artifact under `key` on `layer`, if any.
fn served_row(engine: &Engine, layer: &str, key: &str) -> Option<ArtifactOut> {
    artifacts_of(engine)
        .into_iter()
        .find(|a| a.layer == layer && a.key.as_deref() == Some(key))
}

/// A `PATCH` row: the key, the members joining, and the parts.
fn join(fx: &Fixture, key: &str, sources: std::ops::Range<u64>) -> IncomingGrowth {
    IncomingGrowth::from_entities(key.into(), fx.members(sources))
}

/// Wait for the executor's publication to write at least `want` artifact content extents — the
/// files the filled values live in once the log is gone.
fn content_extents(fx: &Fixture, want: usize) {
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
        let found = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("artifacts-"))
                    && p.to_string_lossy().ends_with(".blocks.bin")
            })
            .count();
        if found >= want {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {found} of {want} content extents were published within 10s"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn remove_the_whole_log(fx: &Fixture) {
    let dir = fx.wal.parent().expect("the log has a directory");
    let stem = fx.wal.file_stem().expect("the log has a stem").to_owned();
    let mut removed = 0usize;
    for entry in std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
    {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{}-", stem.to_string_lossy())) {
            std::fs::remove_file(entry.path()).expect("a log member is removable");
            removed += 1;
        }
    }
    assert!(
        removed > 0,
        "no log member was found to delete — the test would prove nothing"
    );
}

/// **A parent fill on a held artifact is in the next request's cut, and only a parent fill
/// rebuilds the lineage: a page of members joining leaves it held.**
#[test]
fn a_lineage_fill_moves_the_lineage_and_a_growth_does_not() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/tree", HierarchyKind::Nested))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "child", None, 0..200),
            ],
        )
        .unwrap();
    tick(&engine);
    assert_eq!(
        served(&engine, "clusters/tree"),
        vec![("child".to_string(), 200), ("root".to_string(), 300)],
        "two roots, both served"
    );
    let (rows_warm, lineage_warm) = engine.artifact_cache_builds();

    let grown = engine
        .grow_memberships(
            "clusters/tree".into(),
            0,
            vec![join(&fx, "child", 200..300)],
        )
        .expect("a growth");
    assert_eq!((grown[0].joined, grown[0].filled), (100, 0));
    tick(&engine);
    assert_eq!(
        served(&engine, "clusters/tree"),
        vec![("child".to_string(), 300), ("root".to_string(), 300)],
        "the growth is served"
    );
    assert_eq!(
        engine.artifact_cache_builds(),
        (rows_warm, lineage_warm),
        "the growth was taken as a delta by the row form and moved no edge, so the lineage is \
         the one already held"
    );

    let mut fill = join(&fx, "child", 0..0);
    fill.parts.parent_keys = vec!["root".into()];
    let filled = engine
        .grow_memberships("clusters/tree".into(), 0, vec![fill.clone()])
        .expect("a lineage fill");
    assert_eq!((filled[0].joined, filled[0].filled), (0, 1));
    tick(&engine);
    assert_eq!(
        served(&engine, "clusters/tree"),
        vec![("child".to_string(), 300)],
        "the new edge is in the cut: the child covers its root and replaces it"
    );
    assert_eq!(
        engine.artifact_cache_builds().1,
        lineage_warm + 1,
        "the fill rebuilt the lineage once"
    );

    // Identical again: nothing filled, nothing rebuilt.
    let again = engine
        .grow_memberships("clusters/tree".into(), 0, vec![fill])
        .expect("an identical fill is accepted");
    assert_eq!(again[0].filled, 0);
    let before = engine.artifact_cache_builds();
    served(&engine, "clusters/tree");
    assert_eq!(engine.artifact_cache_builds(), before);

    // A fill closing a cycle through the held edge is refused (R4): root under child, when child
    // is already under root.
    let mut other = join(&fx, "root", 0..0);
    other.parts.parent_keys = vec!["child".into()];
    let refused = engine
        .grow_memberships("clusters/tree".into(), 0, vec![other])
        .expect_err("root under child closes root → child → root");
    assert!(refused.to_string().contains("cycle"), "{refused}");

    // The fill comes back from a restart: the record replays through the one store method.
    drop(engine);
    let engine = fx.open();
    assert_eq!(
        served(&engine, "clusters/tree"),
        vec![("child".to_string(), 300)]
    );
}

/// **A differing fixed part is refused naming the part, never the value**, and nothing in the
/// batch lands.
#[test]
fn a_differing_part_is_refused_by_name_and_the_batch_has_no_effect() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/tree", HierarchyKind::Nested))
        .unwrap();
    engine
        .publish_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "root", None, 0..300),
                node(&fx, "other", None, 300..400),
                node(&fx, "child", Some("root"), 0..100),
            ],
        )
        .unwrap();
    let mut moved = join(&fx, "child", 100..150);
    moved.parts.parent_keys = vec!["other".into()];
    let refused = engine
        .grow_memberships("clusters/tree".into(), 0, vec![moved])
        .expect_err("the child holds root");
    let text = refused.to_string();
    assert!(text.contains("parent"), "{text}");
    assert!(
        !text.contains("root"),
        "the held value is the caller's data and is not echoed: {text}"
    );
    assert_eq!(
        served_row(&engine, "clusters/tree", "child")
            .expect("served")
            .masked_count,
        100,
        "the members beside the refused part did not join"
    );
}

/// **An artifact published without the content its layer declares is counted and withheld, and
/// served once the content is filled** (R5) — through a restart, and through the reclaimed log
/// once the content extent has carried the values.
#[test]
fn a_content_fill_serves_a_withheld_artifact_and_outlives_the_log() {
    let fx = fixture();
    {
        let engine = fx.open();
        engine.register_layer(described("topics/a")).unwrap();
        let published = engine
            .put_artifacts(
                "topics/a".into(),
                0,
                vec![IncomingArtifact::from_entities(
                    Some("t0".into()),
                    fx.members(0..100),
                )],
            )
            .expect("accepted without content");
        assert_eq!((published.created, published.without_content), (1, 1));
        assert!(
            served_row(&engine, "topics/a", "t0").is_none(),
            "withheld until it carries the content its layer declares (decision 0076)"
        );

        let mut fill = join(&fx, "t0", 0..0);
        fill.parts.contents = vec![(0, vec!["shipping".into()])];
        let filled = engine
            .grow_memberships("topics/a".into(), 0, vec![fill.clone()])
            .expect("a content fill");
        assert_eq!(filled[0].filled, 1);
        tick(&engine);
        let row = served_row(&engine, "topics/a", "t0").expect("served once filled");
        assert_eq!(row.content, vec!["shipping".to_string()]);
        assert_eq!(row.masked_count, 100);

        // Identical: accepted, nothing filled. Differing: refused naming the rank, not the text.
        assert_eq!(
            engine
                .grow_memberships("topics/a".into(), 0, vec![fill])
                .unwrap()[0]
                .filled,
            0
        );
        let mut differing = join(&fx, "t0", 0..0);
        differing.parts.contents = vec![(0, vec!["logistics".into()])];
        let refused = engine
            .grow_memberships("topics/a".into(), 0, vec![differing])
            .expect_err("a content is written once");
        let text = refused.to_string();
        assert!(text.contains("content[0]"), "{text}");
        assert!(!text.contains("shipping"), "{text}");

        // The publication that follows the fill writes the content extent: the artifact was
        // published without content, so this is the first row its entity has anywhere. The
        // record's own copy of the content, its digest in the membership extent, is rewritten
        // only by the fold, so the log is pinned at the fill until then: two rotations may not
        // reclaim the member holding it.
        content_extents(&fx, 1);
        let holding = wal_members(&fx)
            .pop()
            .expect("the log has at least one member");
        rotate(&engine);
        rotate(&engine);
        assert!(
            wal_members(&fx).contains(&holding),
            "{holding} holds the fill and is still there: rotation may not reclaim past it while \
             the log is the record's only home. Members now: {:?}",
            wal_members(&fx)
        );
    }

    // A restart replays whatever the log still holds.
    {
        let engine = fx.open();
        let row = served_row(&engine, "topics/a", "t0").expect("served after a restart");
        assert_eq!(row.content, vec!["shipping".to_string()]);

        // The fold rewrites the level whole, filled parts included, and releases the log.
        engine
            .accept_change(
                fx.members(900..901)[0],
                tessera_lifecycle::wal::ChangeOp::Delete,
            )
            .unwrap();
        fold(&engine);
    }

    // And with the log gone, the extents are the only home the fill has.
    remove_the_whole_log(&fx);
    let engine = fx.open();
    let row = served_row(&engine, "topics/a", "t0").expect("served from the extents alone");
    assert_eq!(
        row.content,
        vec!["shipping".to_string()],
        "the filled content reached the fold's rewrite and the content extent before the log \
         was released"
    );
}

/// The log's surviving members, oldest first.
fn wal_members(fx: &Fixture) -> Vec<String> {
    let dir = fx.wal.parent().expect("the log has a directory");
    let stem = fx
        .wal
        .file_stem()
        .expect("the log has a stem")
        .to_string_lossy()
        .to_string();
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .expect("the log's directory exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(&format!("{stem}-")) && n.ends_with(".log"))
        .collect();
    found.sort();
    found
}

/// A tick against an empty buffer: nothing to flush, so it rotates the log.
fn rotate(engine: &Engine) {
    let before = engine.write_executor_stats().ticks;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while engine.write_executor_stats().ticks == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the tick that rotates the log never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// **A `PUT` naming a held key mints nothing** (R3): a batch mixing held keys with new ones
/// creates only the new ones, a new artifact under a held sibling lands on the held ordinal, and
/// an identical re-`PUT` is a no-op that rebuilds nothing.
#[test]
fn a_mixed_put_mints_only_the_new_keys_and_an_identical_re_put_is_a_no_op() {
    let fx = fixture();
    let engine = fx.open();
    engine
        .register_layer(declaration("clusters/tree", HierarchyKind::Nested))
        .unwrap();
    let first = engine
        .put_artifacts(
            "clusters/tree".into(),
            0,
            vec![node(&fx, "root", None, 0..300)],
        )
        .unwrap();
    assert_eq!(first.created, 1);
    assert_eq!(engine.published_artifacts(), 1);

    let mixed = engine
        .put_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "leaf", Some("root"), 0..100),
                node(&fx, "root", None, 0..300),
            ],
        )
        .expect("a held key beside a new one");
    assert_eq!((mixed.created, mixed.joined, mixed.filled), (1, 0, 0));
    assert_eq!(
        mixed.tessera_ids[1], first.tessera_ids[0],
        "the held key answers the artifact it names"
    );
    assert_eq!(
        engine.published_artifacts(),
        2,
        "one artifact was minted, not two"
    );
    assert_eq!(
        served(&engine, "clusters/tree"),
        vec![("leaf".to_string(), 100)],
        "the leaf is under the held root and, pruning children, replaces it in the cut"
    );

    // Identical again: no record, no version move, no rebuild.
    let warm = engine.artifact_cache_builds();
    let again = engine
        .put_artifacts(
            "clusters/tree".into(),
            0,
            vec![
                node(&fx, "leaf", Some("root"), 0..100),
                node(&fx, "root", None, 0..300),
            ],
        )
        .expect("an identical re-PUT");
    assert_eq!((again.created, again.joined, again.filled), (0, 0, 0));
    assert_eq!(again.tessera_ids, mixed.tessera_ids);
    assert_eq!(engine.published_artifacts(), 2);
    served(&engine, "clusters/tree");
    assert_eq!(
        engine.artifact_cache_builds(),
        warm,
        "nothing moved, so nothing was derived again"
    );

    // A re-PUT with more members joins them, and a re-PUT naming another parent is the conflict.
    let grown = engine
        .put_artifacts(
            "clusters/tree".into(),
            0,
            vec![node(&fx, "leaf", Some("root"), 0..150)],
        )
        .unwrap();
    assert_eq!((grown.created, grown.joined), (0, 50));
    let refused = engine
        .put_artifacts(
            "clusters/tree".into(),
            0,
            vec![node(&fx, "root", Some("leaf"), 0..300)],
        )
        .expect_err("root under leaf closes a cycle through the held edge");
    assert!(refused.to_string().contains("cycle"), "{refused}");
    assert_eq!(engine.published_artifacts(), 2);
}

/// **A content filled on an artifact the points minted**: an open layer's key arriving on an
/// ingest column creates the artifact, and the enrichment arrives by `PATCH` afterwards
/// (`ingest.md` §1.5, decision 0091).
#[test]
fn a_content_fill_reaches_an_artifact_published_bare_and_survives_a_fold() {
    let fx = fixture();
    let engine = fx.open();
    engine.register_layer(described("topics/a")).unwrap();
    engine
        .put_artifacts(
            "topics/a".into(),
            0,
            vec![
                IncomingArtifact::from_entities(Some("t0".into()), fx.members(0..100)),
                IncomingArtifact::with_content(
                    Some("t1".into()),
                    fx.members(100..200),
                    vec![IncomingContent::new(vec!["described".into()], [])],
                ),
            ],
        )
        .unwrap();
    let mut fill = join(&fx, "t0", 0..0);
    fill.parts.contents = vec![(0, vec!["filled".into()])];
    engine
        .grow_memberships("topics/a".into(), 0, vec![fill])
        .unwrap();

    // A deletion inside `t1`, so the fold rewrites the level whole and carries the content
    // extents forward.
    engine
        .accept_change(
            fx.members(150..151)[0],
            tessera_lifecycle::wal::ChangeOp::Delete,
        )
        .unwrap();
    fold(&engine);
    let mut rows: Vec<(String, Vec<String>, u64)> = artifacts_of(&engine)
        .into_iter()
        .filter(|a| a.layer == "topics/a")
        .map(|a| (a.key.unwrap_or_default(), a.content, a.masked_count))
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("t0".to_string(), vec!["filled".to_string()], 100),
            ("t1".to_string(), vec!["described".to_string()], 99),
        ]
    );

    drop(engine);
    let engine = fx.open();
    let row = served_row(&engine, "topics/a", "t0").expect("served after the fold and a restart");
    assert_eq!(row.content, vec!["filled".to_string()]);
}
