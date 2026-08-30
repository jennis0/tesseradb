//! **A build materialises every declared view** (`views.md` §7), and each of them is its own row
//! space over one entity space.
//!
//! What is checked here is exactly the factoring `views.md` §1 rests on: identity, the label and
//! the term index are shared, and everything downstream of the permutation is the view's. So a
//! shared entity has a *different* position in each view, an entity one view does not hold is
//! sentinel in its permutation and present in the other's, and a view's row count is its own
//! population rather than the entity total.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use tessera_build::{build, BuildArgs, ViewArgs};
use tessera_spatial::Bounds;
use tessera_store::read::open_bundle;
use tessera_types::{EntityId, IdentityKey};

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// `world` holds 0..24; `quarter` holds 12..36. 12..24 is the overlap — the ordinary case
/// (`views.md` §4) — and 24..36 is in `quarter` alone, which is the case a plain build could
/// never produce.
const WORLD: std::ops::Range<u64> = 0..24;
const QUARTER: std::ops::Range<u64> = 12..36;
const ENTITIES: u64 = 36;

fn extent() -> Bounds {
    Bounds {
        x_min: 0.0,
        x_max: 1000.0,
        y_min: 0.0,
        y_max: 1000.0,
    }
}

/// A view's own layout: the same entity sits somewhere quite different in each, which is the
/// whole point of a second view.
fn position(view: &str, e: u64) -> (f64, f64) {
    match view {
        "world" => ((e % 8) as f64 * 100.0, (e / 8) as f64 * 100.0),
        _ => (900.0 - (e % 8) as f64 * 100.0, (e / 8) as f64 * 50.0 + 7.0),
    }
}

fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = ids.collect();
    let xs: Vec<f64> = ids.iter().map(|&e| position(view, e).0).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| position(view, e).1).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The labels, as one exploded relation over **entity** space — shared by both views, which is
/// what `views.md` §7's "the label is the entity's, not the row's" means at a build.
fn write_pairs(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let entities: Vec<u64> = (0..ENTITIES).collect();
    let terms: Vec<u32> = entities.iter().map(|&e| (e % 4) as u32 + 1).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

fn view_args(view: &str, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        access: tessera_build::config::AccessInput::relation(pairs.to_path_buf()),
    }
}

#[test]
fn two_views_are_two_row_spaces_over_one_entity_space() {
    let dir = tempfile::tempdir().unwrap();
    let world_points = dir.path().join("world.parquet");
    let quarter_points = dir.path().join("quarter.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&world_points, "world", WORLD);
    write_points(&quarter_points, "quarter:2026-Q2", QUARTER);
    write_pairs(&pairs);
    let out = dir.path().join("bundle");

    let report = build(&BuildArgs {
        views: vec![
            view_args("world", &world_points, &pairs),
            view_args("quarter:2026-Q2", &quarter_points, &pairs),
        ],
        // `world` is the declared anchor: within a signature group, ids are ordered by the Morton
        // code an item holds *there* (decision 0112).
        anchor: 0,
        groups: vec![tessera_store::manifest::GroupDescriptor {
            name: "quarter".to_string(),
            members_of: None,
            views: vec![tessera_store::manifest::GroupViewDescriptor {
                key: "2026-Q2".to_string(),
                ordinal: 0,
                visibility: None,
                metadata: Default::default(),
            }],
        }],
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a two-view build succeeds");

    // **Entity space is the union, and each row space is its own population.**
    assert_eq!(report.items, ENTITIES);
    assert_eq!(report.views.len(), 2);
    assert_eq!(report.views[0].view_id, "world");
    assert_eq!(report.views[0].rows, WORLD.end - WORLD.start);
    assert_eq!(report.views[1].view_id, "quarter:2026-Q2");
    assert_eq!(report.views[1].rows, QUARTER.end - QUARTER.start);

    let bundle = open_bundle(&out).expect("the bundle opens");
    // The roster is published, and it names the view the manifest declares (`views.md` §3.2).
    assert_eq!(bundle.manifest.groups.len(), 1);
    assert_eq!(bundle.manifest.groups[0].views[0].key, "2026-Q2");
    assert!(bundle
        .manifest
        .views
        .iter()
        .any(|v| v.id == "quarter:2026-Q2"));

    let partition = bundle.partitions.get("default").expect("one partition");
    let world = partition.views.get("world").expect("the plain view");
    let quarter = partition
        .views
        .get("quarter:2026-Q2")
        .expect("the group's view");
    assert_eq!(
        world.segments[0].row_count,
        (WORLD.end - WORLD.start) as u32
    );
    assert_eq!(
        quarter.segments[0].row_count,
        (QUARTER.end - QUARTER.start) as u32
    );

    // **Every entity's membership, and every shared entity's two positions.** Entity ids are
    // assigned in signature order over the union, so which entity a source id became is the
    // build's business; what is checked here is the shape of the two row spaces over the whole of
    // entity space, which needs no such map.
    let (mut both, mut world_only, mut quarter_only) = (0u64, 0u64, 0u64);
    let world_codes = world.segments[0].morton.u32();
    let quarter_codes = quarter.segments[0].morton.u32();
    for entity in 0..ENTITIES {
        let entity = EntityId::new(entity);
        match (
            world.row_space.row_of(entity),
            quarter.row_space.row_of(entity),
        ) {
            (Some(world_row), Some(quarter_row)) => {
                both += 1;
                // **One entity, two layouts.** The same code in both would mean one of the views
                // had quantised the other's coordinates.
                assert_ne!(
                    world_codes[world_row.raw() as usize],
                    quarter_codes[quarter_row.raw() as usize],
                    "entity {entity:?} has one position in both views"
                );
            }
            // Sentinel in one permutation, a row in the other: absent from a row space, present
            // in entity space, with its label and its identity intact (`views.md` §3.4).
            (Some(_), None) => world_only += 1,
            (None, Some(_)) => quarter_only += 1,
            (None, None) => panic!("every entity came from some view's points file"),
        }
    }
    assert_eq!(both, QUARTER.start - WORLD.start);
    assert_eq!(world_only, WORLD.end - QUARTER.start);
    assert_eq!(quarter_only, QUARTER.end - WORLD.end);
}

/// **The label is the entity's, not the row's** (`views.md` §7): a per-view label column that
/// disagrees between two views is a refusal naming the entity, not a union.
#[test]
fn a_label_that_disagrees_between_views_refuses() {
    use arrow::array::StringArray;

    let dir = tempfile::tempdir().unwrap();
    let write = |path: &Path, label_of: &dyn Fn(u64) -> &'static str| {
        let schema = Arc::new(Schema::new(vec![
            Field::new("entity_id", DataType::UInt64, false),
            Field::new("x", DataType::Float64, false),
            Field::new("y", DataType::Float64, false),
            Field::new("access", DataType::Utf8, true),
        ]));
        let ids: Vec<u64> = (0..8).collect();
        let xs: Vec<f64> = ids.iter().map(|&e| e as f64 * 10.0).collect();
        let ys: Vec<f64> = ids.iter().map(|&e| e as f64 * 5.0).collect();
        let labels: Vec<&str> = ids.iter().map(|&e| label_of(e)).collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt64Array::from(ids)),
                Arc::new(Float64Array::from(xs)),
                Arc::new(Float64Array::from(ys)),
                Arc::new(StringArray::from(labels)),
            ],
        )
        .unwrap();
        let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    };
    let a = dir.path().join("a.parquet");
    let b = dir.path().join("b.parquet");
    write(&a, &|_| "public");
    // Entity 5 is `finance` here and `public` there — a widening if the two were unioned.
    write(&b, &|e| if e == 5 { "finance" } else { "public" });

    let field_view = |view: &str, points: &Path| ViewArgs {
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        access: tessera_build::config::AccessInput {
            source: tessera_build::config::AccessSource::Field("access".to_string()),
            default: "public".to_string(),
        },
    };
    let error = build(&BuildArgs {
        views: vec![field_view("a", &a), field_view("b", &b)],
        anchor: 0,
        groups: Vec::new(),
        attribute_sources: Vec::new(),
        out: dir.path().join("bundle"),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect_err("a disagreeing label refuses");
    let message = format!("{error}");
    assert!(message.contains("entity_id 5"), "{message}");
    assert!(message.contains("different access labels"), "{message}");
}
