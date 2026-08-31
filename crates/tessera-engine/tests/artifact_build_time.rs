//! **A layer built into a bundle is the same layer a registration would have made.**
//!
//! The build plane and the control plane run the same registry, the same allocator and the same
//! publication, so what this file checks is the half that neither of those can check on its own:
//! that a *served* bundle comes up with its layers reachable, its clusters counted against the
//! asking principal's own visible set, its labels contained and its edges enforced — and that the
//! ids the build spent are not handed out a second time by the first online registration after it.

mod common;

use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, ListBuilder, StringArray, StringBuilder, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use common::*;
use parquet::arrow::ArrowWriter;
use tessera_build::BuildArgs;
use tessera_engine::{ArtifactOut, Engine, ViewportRequest};
use tessera_lifecycle::wal::ChangeOp;
use tessera_types::{EntityId, TesseraId};

const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const CLUSTERS: &str = "clusters/a";
const LABELS: &str = "topics/x";
/// The cluster's membership, in source ids. Every third source id carries the subset term, so a
/// subset principal sees a third of it — which is what makes the two counts differ.
const MEMBERS: std::ops::Range<u64> = 0..150;

/// Two layers: a `public` clustering, and labels attached into it carrying corpus-derived text.
const CONFIG_TOML: &str = r#"
[sources]
clusters         = "clusters.parquet"
clusters_members = "clusters_members.parquet"
topics           = "topics.parquet"
topics_members   = "topics_members.parquet"

[[view]]
name             = "s0"
extent           = { min = 0.0, max = 1000.0 }
point_visibility = { default = "public" }

[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
source = "clusters"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 2 }
hierarchy = { kind = "flat" }
content = { computed = ["centroid"] }

  [layer.members]
  source = "clusters_members"

[[layer]]
name = "topics/x"
title = "topics"
views = ["s0"]
source = "topics"
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }
depends_on = ["clusters/a"]

  [layer.members]
  source = "topics_members"

  [[layer.content.supplied]]
  name = "topic"
  type = "text"
  require_member_visibility = "all"
"#;

/// The clustering: one row, one artifact, no content and no edges.
fn write_clusters(path: &Path) {
    let schema = Arc::new(Schema::new(vec![Field::new("key", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["c-0000"])) as ArrayRef],
    )
    .unwrap();
    write(path, schema, batch);
}

/// The label layer: one row, whose `contents` is the whole ranking — best first.
fn write_topics(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new(
            "contents",
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                true,
            ))),
            true,
        ),
        Field::new("attached_layer", DataType::Utf8, true),
        Field::new("attached_key", DataType::Utf8, true),
    ]));
    let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
    contents.values().values().append_value("the whole cluster");
    contents.values().append(true);
    contents.values().values().append_value("the subset's own");
    contents.values().append(true);
    contents.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["l-0000"])) as ArrayRef,
            Arc::new(contents.finish()),
            Arc::new(StringArray::from(vec![Some(CLUSTERS)])),
            Arc::new(StringArray::from(vec![Some("c-0000")])),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// One row per `(artifact, entity)` for one layer — the layer being the file's, not a column's.
fn write_members(path: &Path, rows: &[(&str, Option<u32>, u64)]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new("rank", DataType::UInt32, true),
        Field::new("entity", DataType::UInt64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(k, _, _)| *k).collect::<Vec<_>>(),
            )) as ArrayRef,
            Arc::new(UInt32Array::from(
                rows.iter().map(|(_, r, _)| *r).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                rows.iter().map(|(_, _, e)| *e).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

fn write(path: &Path, schema: Arc<Schema>, batch: RecordBatch) {
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn cluster_members() -> Vec<(&'static str, Option<u32>, u64)> {
    MEMBERS.map(|m| ("c-0000", None, m)).collect()
}

fn topic_members() -> Vec<(&'static str, Option<u32>, u64)> {
    let mut rows = Vec::new();
    for m in MEMBERS {
        rows.push(("l-0000", None, m));
        // Rank 0 is generated from the whole cluster — no principal below contains it —
        // and rank 1 from the documents the subset term grants, which the subset principal
        // contains entirely.
        rows.push(("l-0000", Some(0), m));
        if terms_of(m).contains(&SUBSET_TERM) {
            rows.push(("l-0000", Some(1), m));
        }
    }
    rows
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

/// A bundle built **with** its layers and artifacts — no control-plane call anywhere.
fn fixture() -> Fixture {
    try_fixture(write_topics).expect("a build carrying layers")
}

/// The same build, with the label file written by `topics` — so a case about what the build
/// *refuses* runs the whole pipeline the accepted case runs, rather than a reconstruction of it.
fn try_fixture(topics: fn(&Path)) -> Result<Fixture, tessera_build::BuildError> {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let config_path = tmp.path().join("config.toml");
    write_points_n(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);
    std::fs::write(&config_path, CONFIG_TOML).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture config parses");
    write_clusters(&tmp.path().join("clusters.parquet"));
    topics(&tmp.path().join("topics.parquet"));
    write_members(
        &tmp.path().join("clusters_members.parquet"),
        &cluster_members(),
    );
    write_members(&tmp.path().join("topics_members.parquet"), &topic_members());

    let args = BuildArgs {
        views: vec![tessera_build::ViewArgs {
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: root.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: config.layers,
        layer_inputs: config.layer_sources,
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    tessera_build::build(&args)?;
    Ok(Fixture {
        root,
        cache: tmp.path().join("cache"),
        wal: tmp.path().join("wal.log"),
        _tmp: tmp,
    })
}

impl Fixture {
    fn open(&self) -> Engine {
        open_engine_publishing(&self.root, &self.cache, &self.wal)
    }
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

fn of_layer<'a>(served: &'a [ArtifactOut], layer: &str) -> Vec<&'a ArtifactOut> {
    served.iter().filter(|a| a.layer == layer).collect()
}

fn artifact_entity(engine: &Engine, id: TesseraId) -> EntityId {
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine.resolve_tessera_ids(&[id], idset).unwrap()[0].expect("it names what was issued")
}

/// **The headline.** A bundle that has never seen a control-plane call serves its clusters with
/// each principal's own masked count, and its labels with the description that principal contains.
#[test]
fn a_built_layer_serves_with_masked_counts_and_contained_content() {
    let fx = fixture();
    let engine = fx.open();

    let broad = artifacts_of(&engine, &full_coverage_credential());
    let cluster = of_layer(&broad, CLUSTERS);
    assert_eq!(cluster.len(), 1, "the built clustering is served");
    assert_eq!(cluster[0].key.as_deref(), Some("c-0000"));
    assert_eq!(cluster[0].masked_count, MEMBERS.count() as u64);
    assert!(
        cluster[0].derived.centroid.is_some(),
        "a declared derived property is computed from the viewer's own visible members"
    );
    // Every member visible, so the whole-cluster description is contained.
    assert_eq!(
        of_layer(&broad, LABELS)[0].content,
        vec!["the whole cluster".to_string()]
    );

    // A third of the corpus visible: a different count for the same cluster, and the narrower
    // description — the one generated from exactly what this principal can see.
    let narrow = artifacts_of(&engine, &subset_credential());
    let narrow_cluster = of_layer(&narrow, CLUSTERS);
    let visible = MEMBERS
        .filter(|m| terms_of(*m).contains(&SUBSET_TERM))
        .count() as u64;
    assert_eq!(narrow_cluster[0].masked_count, visible);
    assert_ne!(narrow_cluster[0].masked_count, cluster[0].masked_count);
    assert_eq!(
        of_layer(&narrow, LABELS)[0].content,
        vec!["the subset's own".to_string()]
    );
}

/// The edge is a visibility term wherever it was written: suppress the built cluster and its
/// built label stops serving, on the viewport and on the identifier route alike.
#[test]
fn a_built_edge_withholds_its_label_when_the_cluster_is_suppressed() {
    let fx = fixture();
    let engine = fx.open();
    let served = artifacts_of(&engine, &full_coverage_credential());
    let label_id = of_layer(&served, LABELS)[0].tessera_id;
    let cluster_id = of_layer(&served, CLUSTERS)[0].tessera_id;
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(engine
        .artifact(&session, label_id, None, "s0", None)
        .unwrap()
        .is_some());

    engine
        .accept_change(artifact_entity(&engine, cluster_id), ChangeOp::Suppress)
        .unwrap();

    let after = artifacts_of(&engine, &full_coverage_credential());
    assert!(
        of_layer(&after, LABELS).is_empty(),
        "the label goes with its cluster"
    );
    assert!(engine
        .artifact(&session, label_id, None, "s0", None)
        .unwrap()
        .is_none());
}

/// **A built bundle can be published into, and neither publication loses the other.** The build
/// writes its membership extent into the same directory a later online publication writes to, and
/// the two name their files from different counters — so this is the test that says the counters do
/// not meet. A collision would overwrite a built extent in place while the manifest still named it,
/// and the built artifacts would come back carrying another publication's members.
#[test]
fn a_built_bundle_takes_an_online_publication_beside_its_own() {
    let fx = fixture();
    let published_id = {
        let engine = fx.open();
        let map = source_to_new_map(&fx.root, "v00000");
        let entities: Vec<EntityId> = MEMBERS
            .filter(|m| terms_of(*m).contains(&SUBSET_TERM))
            .map(|m| EntityId::new(map[&m]))
            .collect();
        engine
            .publish_artifacts(
                CLUSTERS.into(),
                0,
                vec![tessera_lifecycle::IncomingArtifact::from_entities(
                    Some("c-online".into()),
                    entities,
                )],
            )
            .expect("a built layer takes a publication")[0]
    };

    // Reopened: the built cluster, the built label and the online cluster all serve.
    let engine = fx.open();
    let served = artifacts_of(&engine, &full_coverage_credential());
    let keys: Vec<&str> = served.iter().filter_map(|a| a.key.as_deref()).collect();
    for expected in ["c-0000", "l-0000", "c-online"] {
        assert!(keys.contains(&expected), "{expected} missing from {keys:?}");
    }
    // The built cluster still holds the members the build gave it — an extent overwritten by the
    // online publication would show up here as a count from the wrong membership.
    let built = served
        .iter()
        .find(|a| a.key.as_deref() == Some("c-0000"))
        .unwrap();
    assert_eq!(built.masked_count, MEMBERS.count() as u64);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(engine
        .artifact(&session, published_id, None, "s0", None)
        .unwrap()
        .is_some());
}

/// **The mark the build spent must survive into the manifest**, or the first online registration
/// is handed ids the built layers already hold — two entities under one `tessera_id`.
#[test]
fn a_later_online_registration_does_not_reissue_the_builds_ids() {
    let fx = fixture();
    let engine = fx.open();
    let built: Vec<EntityId> = artifacts_of(&engine, &full_coverage_credential())
        .iter()
        .map(|a| artifact_entity(&engine, a.tessera_id))
        .collect();
    assert_eq!(built.len(), 2);

    let id = engine
        .register_layer(tessera_types::layer::LayerDeclaration {
            name: "clusters/online".into(),
            title: Some("registered against the running node".into()),
            views: vec!["s0".into()],
            membership: tessera_types::layer::MembershipSource::Enumerated,
            value_set: Default::default(),
            visibility: None,
            artifact_visibility: tessera_types::layer::ArtifactVisibility::inherited(),
            require_member_visibility: None,
            hierarchy: tessera_types::layer::Hierarchy {
                kind: tessera_types::layer::HierarchyKind::Flat,
                prune_children: false,
            },
            content: Default::default(),
            depends_on: Vec::new(),
            levels: Vec::new(),
            layout: None,
            shape: None,
        })
        .expect("a layer registers against a bundle that already carries some");
    let online = artifact_entity(&engine, id);
    assert!(
        !built.contains(&online),
        "an entity a built artifact holds was handed to a new layer"
    );
    // And it lands below them, the row-less region growing downward from where the build left it.
    assert!(built.iter().all(|e| online.raw() < e.raw()));

    // The built layers are still there and still reachable after the registration.
    let names: Vec<String> = engine
        .visible_layers(&engine.authorise(&full_coverage_credential()).unwrap())
        .into_iter()
        .map(|l| l.declaration.name)
        .collect();
    for expected in [CLUSTERS, LABELS, "clusters/online"] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} missing from {names:?}"
        );
    }
}

/// **A build writes the containment partitions a fold writes**, and the first request that gates a
/// description claims one rather than composing it.
///
/// Composing a partition is a pass over every term's posting for the entities a level's generating
/// sets name — level-scale work that used to land on whichever request arrived first after a boot
/// (`docs/evidence/memos/2026-08-22-artifact-scale-campaign.md`, finding 2's family). The build has
/// the store, the postings and the prefix in hand, so it does it once.
#[test]
fn a_built_bundle_carries_its_containment_partitions_and_the_first_request_claims_one() {
    let fx = fixture();
    let engine = fx.open();
    let generation = engine.generation();
    let manifest = &generation
        .bundle
        .partitions
        .values()
        .next()
        .expect("one partition")
        .manifest;
    assert!(
        manifest
            .containment_extents
            .iter()
            .any(|e| e.layer == LABELS),
        "the label layer's generating sets have a partition the build composed: {:?}",
        manifest.containment_extents
    );

    // The gauges: one adopted at the open, none composed by the request that used it.
    let served = artifacts_of(&engine, &full_coverage_credential());
    assert_eq!(
        of_layer(&served, LABELS)[0].content,
        vec!["the whole cluster".to_string()],
        "the same answer the composed partition gives"
    );
    assert!(engine.artifact_containment_partitions_adopted() > 0);
    assert_eq!(
        engine.artifact_containment_partitions(),
        0,
        "a partition the build already composed was composed a second time on the request path"
    );
}

/// The label layer's file with its edge columns left out — one label, attached to nothing.
fn write_topics_without_edges(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("key", DataType::Utf8, false),
        Field::new(
            "contents",
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
                true,
            ))),
            true,
        ),
    ]));
    let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
    contents.values().values().append_value("the whole cluster");
    contents.values().append(true);
    contents.values().values().append_value("the subset's own");
    contents.values().append(true);
    contents.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["l-0000"])) as ArrayRef,
            Arc::new(contents.finish()),
        ],
    )
    .unwrap();
    write(path, schema, batch);
}

/// **The build refuses what the ingest refuses** — an artifact declaring no dependency in a layer
/// that declares one ([decision 0089](../../../docs/decisions/0089-a-dependency-edge-carries-deletion-and-visibility.md)).
///
/// A dependent is served only where the artifact it attaches to is served, so a label with no
/// attachment has nothing for that prerequisite to gate on: it would serve on its own conjuncts
/// alone, over a cluster layer that gates every one of its clusters. A build that admitted it while
/// the control plane refused it is the fail-open half of one rule stated twice.
#[test]
fn the_build_refuses_a_label_that_attaches_to_nothing() {
    let err = try_fixture(write_topics_without_edges)
        .err()
        .expect("a label layer's artifacts attach to something");
    let message = format!("{err}");
    assert!(message.contains("attaches to nothing"), "{message}");
    assert!(message.contains("topics/x"), "{message}");
}
