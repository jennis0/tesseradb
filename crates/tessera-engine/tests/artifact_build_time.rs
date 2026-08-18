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

/// Two layers: an ungated clustering, and labels attached into it carrying corpus-derived text.
const CONFIG_TOML: &str = r#"
[[view]]
name             = "s0"
point_visibility = { default = "public" }

[[layer]]
name = "clusters/a"
title = "clusters"
views = ["s0"]
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = { count = 2 }
hierarchy = { kind = "flat" }
content = { computed = ["centroid"] }

[[layer]]
name = "topics/x"
title = "topics"
views = ["s0"]
membership = "enumerated"
visibility = "public"
artifact_visibility = { default = "inherited" }
require_member_visibility = "none"
hierarchy = { kind = "flat" }
depends_on = ["clusters/a"]

[[layer.content.supplied]]
name = "topic"
type = "text"
require_member_visibility = "all"
"#;

fn write_artifacts(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("key", DataType::Utf8, false),
        Field::new("variation", DataType::UInt32, true),
        Field::new(
            "values",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
        Field::new("attached_layer", DataType::Utf8, true),
        Field::new("attached_key", DataType::Utf8, true),
    ]));
    let mut values = ListBuilder::new(StringBuilder::new());
    values.append(false);
    values.values().append_value("the whole cluster");
    values.append(true);
    values.values().append_value("the subset's own");
    values.append(true);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec![CLUSTERS, LABELS, LABELS])) as ArrayRef,
            Arc::new(StringArray::from(vec!["c-0000", "l-0000", "l-0000"])),
            Arc::new(UInt32Array::from(vec![None, Some(0), Some(1)])),
            Arc::new(values.finish()),
            Arc::new(StringArray::from(vec![None, Some(CLUSTERS), Some(CLUSTERS)])),
            Arc::new(StringArray::from(vec![None, Some("c-0000"), Some("c-0000")])),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_members(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("layer", DataType::Utf8, false),
        Field::new("key", DataType::Utf8, false),
        Field::new("variation", DataType::UInt32, true),
        Field::new("member", DataType::UInt64, false),
    ]));
    let (mut layers, mut keys, mut variation, mut member) =
        (Vec::new(), Vec::new(), Vec::<Option<u32>>::new(), Vec::new());
    let mut row = |layer: &str, key: &str, v: Option<u32>, m: u64| {
        layers.push(layer.to_string());
        keys.push(key.to_string());
        variation.push(v);
        member.push(m);
    };
    for m in MEMBERS {
        row(CLUSTERS, "c-0000", None, m);
        row(LABELS, "l-0000", None, m);
        // Variation 0 is generated from the whole cluster — no principal below contains it —
        // and variation 1 from the documents the subset term grants, which the subset principal
        // contains entirely.
        row(LABELS, "l-0000", Some(0), m);
        if terms_of(m).contains(&SUBSET_TERM) {
            row(LABELS, "l-0000", Some(1), m);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(layers)) as ArrayRef,
            Arc::new(StringArray::from(keys)),
            Arc::new(UInt32Array::from(variation)),
            Arc::new(UInt64Array::from(member)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
}

/// A bundle built **with** its layers and artifacts — no control-plane call anywhere.
fn fixture() -> Fixture {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    let config_path = tmp.path().join("config.toml");
    let artifacts = tmp.path().join("artifacts.parquet");
    let members = tmp.path().join("members.parquet");
    write_points_n(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);
    std::fs::write(&config_path, CONFIG_TOML).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the fixture config parses");
    write_artifacts(&artifacts);
    write_members(&members);

    let args = BuildArgs {
        points,
        pairs,
        out: root.clone(),
        extent: extent(),
        view_id: "s0".to_string(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: config.layers,
        artifacts: Some(artifacts),
        artifact_members: Some(members),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    };
    tessera_build::build(&args).expect("a build carrying layers");
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
    assert_eq!(cluster[0].stable_key.as_deref(), Some("c-0000"));
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
    let visible = MEMBERS.filter(|m| terms_of(*m).contains(&SUBSET_TERM)).count() as u64;
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
    assert!(engine.artifact(&session, label_id, None, "s0").unwrap().is_some());

    engine
        .accept_change(artifact_entity(&engine, cluster_id), ChangeOp::Suppress)
        .unwrap();

    let after = artifacts_of(&engine, &full_coverage_credential());
    assert!(of_layer(&after, LABELS).is_empty(), "the label goes with its cluster");
    assert!(engine
        .artifact(&session, label_id, None, "s0")
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
    let keys: Vec<&str> = served
        .iter()
        .filter_map(|a| a.stable_key.as_deref())
        .collect();
    for expected in ["c-0000", "l-0000", "c-online"] {
        assert!(keys.contains(&expected), "{expected} missing from {keys:?}");
    }
    // The built cluster still holds the members the build gave it — an extent overwritten by the
    // online publication would show up here as a count from the wrong membership.
    let built = served
        .iter()
        .find(|a| a.stable_key.as_deref() == Some("c-0000"))
        .unwrap();
    assert_eq!(built.masked_count, MEMBERS.count() as u64);
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    assert!(engine
        .artifact(&session, published_id, None, "s0")
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
            title: "registered against the running node".into(),
            views: vec!["s0".into()],
            membership: tessera_types::layer::MembershipSource::Enumerated,
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
        assert!(names.iter().any(|n| n == expected), "{expected} missing from {names:?}");
    }
}
