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

/// This file's fixtures name their rows by an integer `entity_id` column (`tessera_build::ids`).
static INTEGER_IDS: tessera_build::ids::IdSpace = tessera_build::ids::IdSpace::Integer;

const TEST_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f";

/// `world` holds 0..24; `quarter` holds 12..36. 12..24 is the overlap — the ordinary case
/// (`views.md` §4) — and 24..36 is in `quarter` alone, which is the case a plain build could
/// never produce.
const WORLD: std::ops::Range<u64> = 0..24;
const QUARTER: std::ops::Range<u64> = 12..36;
const ENTITIES: u64 = 36;

/// The same frame as [`extent`], in the manifest's own shape — every view of a group shares one
/// (`views.md` §3.1), which is what the group's own copy records.
fn group_frame() -> tessera_store::manifest::Quantisation {
    let e = extent();
    tessera_store::manifest::Quantisation {
        x_min: e.x_min,
        x_max: e.x_max,
        y_min: e.y_min,
        y_max: e.y_max,
    }
}

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
        visibility: None,
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
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
            title: None,
            name: "quarter".to_string(),
            members_of: None,
            point_default: Some("public".to_string()),
            visibility: None,
            scoped_scalars: Vec::new(),
            quantisation: group_frame(),
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            views: vec![tessera_store::manifest::GroupViewDescriptor {
                key: "2026-Q2".to_string(),
                visibility: None,
                metadata: Default::default(),
            }],
        }],
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
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
        visibility: None,
        view_id: view.to_string(),
        projection: tessera_spatial::Projection::None,
        extent: extent(),
        points: points.to_path_buf(),
        point_fields: Default::default(),
        select: None,
        access: tessera_build::config::AccessInput {
            source: tessera_build::config::AccessSource::Field("access".to_string()),
            default: Some("public".to_string()),
        },
    };
    let error = build(&BuildArgs {
        views: vec![field_view("a", &a), field_view("b", &b)],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: dir.path().join("bundle"),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
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

/// **Form B: one file, a discriminator column** (`views.md` §3.1). Each view's rows are picked
/// out of the shared source by its key, so the two row spaces are the two halves of one file and
/// neither reads the other's rows.
#[test]
fn a_discriminator_selects_each_views_rows_out_of_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("quarter-alt.parquet");
    // Q2 holds 0..24 and Q3 holds 12..36 — the same overlap the file-per-view case has, written
    // as one file with a key per row.
    write_discriminated(
        &points,
        &[("2026-Q2", WORLD), ("2026-Q3", QUARTER)],
    );
    let pairs = dir.path().join("pairs.parquet");
    write_pairs(&pairs);
    let out = dir.path().join("bundle");

    let report = build(&BuildArgs {
        views: vec![
            selected_view("quarter_alt:2026-Q2", "2026-Q2", &points, &pairs),
            selected_view("quarter_alt:2026-Q3", "2026-Q3", &points, &pairs),
        ],
        anchor: 0,
        groups: vec![alt_group(&["2026-Q2", "2026-Q3"])],
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a form B build succeeds");

    // Entity space is the union of the two selections, and each row space is its own selection.
    assert_eq!(report.items, ENTITIES);
    assert_eq!(report.views[0].rows, WORLD.end - WORLD.start);
    assert_eq!(report.views[1].rows, QUARTER.end - QUARTER.start);

    let bundle = open_bundle(&out).expect("the bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let q2 = partition.views.get("quarter_alt:2026-Q2").expect("Q2");
    let q3 = partition.views.get("quarter_alt:2026-Q3").expect("Q3");
    let (mut both, mut q2_only, mut q3_only) = (0u64, 0u64, 0u64);
    for entity in 0..ENTITIES {
        let entity = EntityId::new(entity);
        match (q2.row_space.row_of(entity), q3.row_space.row_of(entity)) {
            (Some(_), Some(_)) => both += 1,
            (Some(_), None) => q2_only += 1,
            (None, Some(_)) => q3_only += 1,
            (None, None) => panic!("every entity came from some view's rows"),
        }
    }
    assert_eq!(both, QUARTER.start - WORLD.start);
    assert_eq!(q2_only, WORLD.end - QUARTER.start);
    assert_eq!(q3_only, QUARTER.end - WORLD.end);
}

/// A row naming a key the roster does not carry belongs to no view, and every view's own
/// selection would skip it — so it is refused naming the key and the roster (`views.md` §3.1).
#[test]
fn a_discriminator_value_outside_the_roster_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("quarter-alt.parquet");
    write_discriminated(&points, &[("2026-Q2", 0..8), ("2026-Q9", 8..12)]);
    let pairs = dir.path().join("pairs.parquet");
    write_pairs(&pairs);

    let error = build(&BuildArgs {
        views: vec![selected_view(
            "quarter_alt:2026-Q2",
            "2026-Q2",
            &points,
            &pairs,
        )],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: dir.path().join("bundle"),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect_err("a key nobody declared refuses");
    let message = format!("{error}");
    assert!(message.contains("2026-Q9"), "{message}");
    assert!(message.contains("2026-Q2"), "{message}");
}

/// One file holding several views' points, `quarter` saying which view each row is in.
fn write_discriminated(path: &Path, blocks: &[(&str, std::ops::Range<u64>)]) {
    use arrow::array::StringArray;
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("quarter", DataType::Utf8, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let mut ids: Vec<u64> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut xs: Vec<f64> = Vec::new();
    let mut ys: Vec<f64> = Vec::new();
    for (key, range) in blocks {
        for e in range.clone() {
            let (x, y) = position(key, e);
            ids.push(e);
            keys.push((*key).to_string());
            xs.push(x);
            ys.push(y);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(StringArray::from(keys)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The roster the manifest publishes for the form B group these tests build.
fn alt_group(keys: &[&str]) -> tessera_store::manifest::GroupDescriptor {
    tessera_store::manifest::GroupDescriptor {
        title: None,
        name: "quarter_alt".to_string(),
        members_of: None,
        point_default: Some("public".to_string()),
        visibility: None,
        scoped_scalars: Vec::new(),
        quantisation: group_frame(),
        projection: tessera_spatial::Projection::None,
        metadata: Vec::new(),
        views: keys
            .iter()
            .map(|key| tessera_store::manifest::GroupViewDescriptor {
                key: (*key).to_string(),
                visibility: None,
                metadata: Default::default(),
            })
            .collect(),
    }
}

fn selected_view(view: &str, key: &str, points: &Path, pairs: &Path) -> ViewArgs {
    ViewArgs {
        select: Some(tessera_build::config::ViewSelector {
            column: "quarter".to_string(),
            value: key.to_string(),
            keys: vec!["2026-Q2".to_string(), "2026-Q3".to_string()],
            view_id: view.to_string(),
        }),
        ..view_args(view, points, pairs)
    }
}

/// **One frame for the group, surveyed over every view's source** (`views.md` §3.1): `auto` on a
/// `[[view_group]]` fits one box to the union of its views' boxes, so a Morton prefix means the
/// same thing in each of them. Fitting per file would give each view its own grid under one
/// declaration.
#[test]
fn an_auto_frame_over_a_group_fits_every_views_source() {
    use tessera_build::config::{frame_of, Extent, FrameSource};

    let dir = tempfile::tempdir().unwrap();
    let q2 = dir.path().join("q2.parquet");
    let q3 = dir.path().join("q3.parquet");
    // Two disjoint boxes: Q2's points are the low corner, Q3's the high one.
    write_points(&q2, "world", 0..8);
    write_points(&q3, "quarter", 24..36);
    let fields = Default::default();
    let (q2, q3) = (q2.as_path(), q3.as_path());
    let of = |points| FrameSource {
        points,
        fields: &fields,
        select: None,
    };

    let group = frame_of(
        "view group 'quarter'",
        tessera_spatial::Projection::None,
        &Extent::Auto { margin: 0.0 },
        &[of(q2), of(q3)],
        None,
    )
    .expect("one frame over both sources");
    let alone = frame_of(
        "view 'quarter:2026-Q2'",
        tessera_spatial::Projection::None,
        &Extent::Auto { margin: 0.0 },
        &[of(q2)],
        None,
    )
    .expect("one frame over one source");

    // The group's box holds both views' data; the single view's does not hold the other's.
    for path in [q2, q3] {
        let rows = tessera_build::input::read_points(
            tessera_build::input::Source::new(path, &fields, &INTEGER_IDS),
            tessera_spatial::Projection::None,
            &group.extent,
        )
        .expect("points read");
        assert!(!rows.is_empty());
    }
    assert!(
        group.extent.x_max > alone.extent.x_max || group.extent.y_max > alone.extent.y_max,
        "the group's frame {:?} is no wider than one view's {:?}",
        group.extent,
        alone.extent
    );
    // Nothing clamps: the box was fitted to every row it will place.
    assert!(group.refusal().is_none());
}

/// **The roster as a table** (`views.md` §3.1's form B): the keys are rows of a file, read before
/// pass two, and each becomes a view of the group with its typed metadata and its own selection
/// out of the shared points file, in the order the table lists them.
#[test]
fn a_roster_table_enumerates_the_groups_views() {
    use arrow::array::{Int64Array, StringArray};

    let dir = tempfile::tempdir().unwrap();
    write_discriminated(
        &dir.path().join("quarter-alt.parquet"),
        &[("2026-Q2", WORLD), ("2026-Q3", QUARTER)],
    );
    // One row per view: the key, its own gate, and the group's declared metadata.
    let roster = dir.path().join("roster.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("quarter", DataType::Utf8, false),
        Field::new("visibility", DataType::Utf8, true),
        Field::new("label", DataType::Utf8, false),
        Field::new("starts", DataType::Int64, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["2026-Q2", "2026-Q3"])),
            Arc::new(StringArray::from(vec![Some("public"), None])),
            Arc::new(StringArray::from(vec!["Q2 2026", "Q3 2026"])),
            Arc::new(Int64Array::from(vec![1_775_001_600_000_000i64, 1i64])),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(&roster).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let config = dir.path().join("corpus.toml");
    std::fs::write(
        &config,
        r#"
[sources]
alt    = "quarter-alt.parquet"
roster = "roster.parquet"

[defaults]
allocation_view = "quarter:2026-Q2"

[[view_group]]
name             = "quarter"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
source           = "alt"
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us" }

[view_group.views]
source = "roster"
fields = { key = "quarter" }
"#,
    )
    .unwrap();
    let config = tessera_build::config::Config::parse(&config, &Default::default())
        .expect("the declaration parses");
    let registry = config.build_views().expect("the roster enumerates");

    assert_eq!(
        registry.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(),
        ["quarter:2026-Q2", "quarter:2026-Q3"]
    );
    let group = registry[1].group.as_ref().expect("a group's view");
    assert_eq!(group.key, "2026-Q3", "the roster's own order is the registry's");
    assert_eq!(
        group.metadata.get("label"),
        Some(&tessera_build::config::MetadataValue::Text(
            "Q3 2026".to_string()
        ))
    );
    assert_eq!(
        group.metadata.get("starts"),
        Some(&tessera_build::config::MetadataValue::TimestampUs(1))
    );
    // Every view's points are the group's one file, selected by its key, and the selection
    // carries the whole roster so a stray key can be refused naming it.
    let select = registry[0].select.as_ref().expect("form B selects");
    assert_eq!(select.column, "quarter");
    assert_eq!(select.value, "2026-Q2");
    assert_eq!(select.keys, ["2026-Q2", "2026-Q3"]);
    // The anchor names a view of the group, which is a view id like any other (`views.md` §3.2).
    assert_eq!(config.anchor_view(&registry).expect("the anchor"), 0);
}

/// **A roster table's integer that does not fit its declared width is refused**, naming the row
/// and the column, and one that fits is read as written. The column is `int64` both times, so the
/// width checked is the declaration's rather than the file's.
#[test]
fn a_roster_tables_integer_past_its_declared_width_is_refused() {
    use arrow::array::{Int64Array, StringArray};

    for (tier, fits) in [(300i64, false), (255, true)] {
        let dir = tempfile::tempdir().unwrap();
        write_discriminated(
            &dir.path().join("quarter-alt.parquet"),
            &[("2026-Q2", WORLD), ("2026-Q3", QUARTER)],
        );
        let roster = dir.path().join("roster.parquet");
        let schema = Arc::new(Schema::new(vec![
            Field::new("quarter", DataType::Utf8, false),
            Field::new("tier", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(StringArray::from(vec!["2026-Q2", "2026-Q3"])),
                Arc::new(Int64Array::from(vec![1, tier])),
            ],
        )
        .unwrap();
        let mut writer =
            ArrowWriter::try_new(File::create(&roster).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();

        let config = dir.path().join("corpus.toml");
        std::fs::write(
            &config,
            r#"
[sources]
alt    = "quarter-alt.parquet"
roster = "roster.parquet"

[defaults]
allocation_view = "quarter:2026-Q2"

[[view_group]]
name             = "quarter"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
source           = "alt"
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }
metadata         = { tier = "u8" }

[view_group.views]
source = "roster"
fields = { key = "quarter" }
"#,
        )
        .unwrap();
        let config = tessera_build::config::Config::parse(&config, &Default::default())
            .expect("the declaration parses");
        let registry = config.build_views();
        if !fits {
            assert!(registry.is_err(), "{tier} does not fit a u8 and is refused");
            continue;
        }
        let registry = registry.expect("a value that fits is read");
        assert_eq!(
            registry[1].group.as_ref().unwrap().metadata.get("tier"),
            Some(&tessera_build::config::MetadataValue::Int(tier))
        );
    }
}

/// **A roster table's `visibility` column may be a list, at either width** (`views.md` §6,
/// decision 0132): each element is one label taken verbatim, a comma included, and the list
/// width and the string width are the writer's choice. `large_list<large_utf8>` is the widest
/// spelling and the one a reader of `list<utf8>` alone would refuse.
#[test]
fn a_roster_tables_gate_column_may_be_a_large_list_of_large_strings() {
    use arrow::array::{Int64Array, LargeListArray, LargeStringArray, StringArray};
    use arrow::buffer::OffsetBuffer;

    let dir = tempfile::tempdir().unwrap();
    write_discriminated(
        &dir.path().join("quarter-alt.parquet"),
        &[("2026-Q2", WORLD), ("2026-Q3", QUARTER), ("2026-Q4", QUARTER)],
    );
    let roster = dir.path().join("roster.parquet");
    let gate_field = Arc::new(Field::new("item", DataType::LargeUtf8, false));
    let schema = Arc::new(Schema::new(vec![
        Field::new("quarter", DataType::Utf8, false),
        Field::new("visibility", DataType::LargeList(gate_field.clone()), true),
        Field::new("label", DataType::Utf8, false),
        Field::new("starts", DataType::Int64, false),
    ]));
    // Q2: two labels; Q3: one label with a comma in it; Q4: null, so the group's gate.
    let labels = LargeStringArray::from(vec!["finance", "legal", "finance,legal"]);
    let gates = LargeListArray::try_new(
        gate_field,
        OffsetBuffer::new(vec![0i64, 2, 3, 3].into()),
        Arc::new(labels),
        Some(vec![true, true, false].into()),
    )
    .unwrap();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["2026-Q2", "2026-Q3", "2026-Q4"])),
            Arc::new(gates),
            Arc::new(StringArray::from(vec!["Q2", "Q3", "Q4"])),
            Arc::new(Int64Array::from(vec![1i64, 2, 3])),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(&roster).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();

    let config = dir.path().join("corpus.toml");
    std::fs::write(
        &config,
        r#"
[sources]
alt    = "quarter-alt.parquet"
roster = "roster.parquet"

[defaults]
allocation_view = "quarter:2026-Q2"

[[view_group]]
name             = "quarter"
extent           = { x = [0.0, 1000.0], y = [0.0, 1000.0] }
source           = "alt"
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text", starts = "timestamp_us" }

[view_group.views]
source = "roster"
fields = { key = "quarter" }
"#,
    )
    .unwrap();
    let config = tessera_build::config::Config::parse(&config, &Default::default())
        .expect("the declaration parses");
    let registry = config.build_views().expect("the roster enumerates");
    let gate_of = |i: usize| registry[i].visibility.clone();
    assert_eq!(
        gate_of(0),
        Some(vec!["finance".to_string(), "legal".to_string()]),
        "a two-element list is two terms"
    );
    assert_eq!(
        gate_of(1),
        Some(vec!["finance,legal".to_string()]),
        "a comma inside an element is part of the label"
    );
    assert_eq!(gate_of(2), None, "a null row takes the group's gate");
}

/// **A listed key with no rows is an empty view** (`views.md` §3.1) — declared, materialised, and
/// holding nobody: its permutation is sentinel everywhere and its segment has no rows.
#[test]
fn a_roster_key_with_no_rows_is_an_empty_view() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("quarter-alt.parquet");
    write_discriminated(&points, &[("2026-Q2", 0..ENTITIES)]);
    let pairs = dir.path().join("pairs.parquet");
    write_pairs(&pairs);
    let out = dir.path().join("bundle");

    let report = build(&BuildArgs {
        views: vec![
            selected_view("quarter_alt:2026-Q2", "2026-Q2", &points, &pairs),
            selected_view("quarter_alt:2026-Q3", "2026-Q3", &points, &pairs),
        ],
        anchor: 0,
        groups: vec![alt_group(&["2026-Q2", "2026-Q3"])],
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("an empty view builds");

    assert_eq!(report.views[1].rows, 0);
    let bundle = open_bundle(&out).expect("the bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let empty = partition.views.get("quarter_alt:2026-Q3").expect("Q3");
    assert_eq!(empty.segments[0].row_count, 0);
    for entity in 0..ENTITIES {
        assert!(empty.row_space.row_of(EntityId::new(entity)).is_none());
    }
}

/// **A group-scoped attribute is a family of entity-space columns** (`views.md` §5): one per view
/// of the group, each with its own presence bitmap (decision 0064), read from that view's own
/// rows. Here the group is form B — one points file, a discriminator — so the two columns are two
/// selections of one file, which is the case a form A group's file-per-view never exercises.
#[test]
fn a_group_scoped_attribute_is_one_column_per_view_of_the_group() {
    use arrow::array::{Float32Array, StringArray};
    use tessera_spatial::tiler::ScalarType;

    let dir = tempfile::tempdir().unwrap();
    // Q2 holds 0..24 and Q3 12..36, and a row's sentiment is null on every third entity — so the
    // two columns carry different values *and* different presence for the entities they share.
    let points = dir.path().join("quarter-alt.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("quarter", DataType::Utf8, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("sentiment", DataType::Float32, true),
    ]));
    let mut ids: Vec<u64> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    let mut sentiment: Vec<Option<f32>> = Vec::new();
    for (key, range) in [("2026-Q2", WORLD), ("2026-Q3", QUARTER)] {
        for e in range {
            let (x, y) = position(key, e);
            ids.push(e);
            keys.push(key.to_string());
            xs.push(x);
            ys.push(y);
            sentiment.push((e % 3 != 0).then_some(if key == "2026-Q2" { 0.5 } else { -0.5 }));
        }
    }
    let q2_present = WORLD.filter(|e| e % 3 != 0).count() as u64;
    let q3_present = QUARTER.filter(|e| e % 3 != 0).count() as u64;
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(StringArray::from(keys)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(Float32Array::from(sentiment)),
        ],
    )
    .unwrap();
    let mut writer = ArrowWriter::try_new(File::create(&points).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let pairs = dir.path().join("pairs.parquet");
    write_pairs(&pairs);
    let out = dir.path().join("bundle");

    let sentiment = tessera_build::config::Attribute {
        name: "sentiment".to_string(),
        title: None,
        field: None,
        ty: ScalarType::F32,
        analyser: None,
        vocabulary: None,
        value_set: None,
        index: true,
        render: false,
    };
    build(&BuildArgs {
        views: vec![
            selected_view("quarter_alt:2026-Q2", "2026-Q2", &points, &pairs),
            selected_view("quarter_alt:2026-Q3", "2026-Q3", &points, &pairs),
        ],
        anchor: 0,
        groups: vec![alt_group(&["2026-Q2", "2026-Q3"])],
        scoped_attributes: vec![tessera_build::ScopedColumnFamily {
            attribute: sentiment,
            group: "quarter_alt".to_string(),
            views: vec![0, 1],
            source: None,
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
        scoped_layers: Default::default(),
        mint_external_ids: false,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a scoped family builds");

    let bundle = open_bundle(&out).expect("the bundle opens");
    assert!(bundle.manifest.declared_scalars.is_empty());
    let attrs = out
        .join("v00000")
        .join("partitions/default/attrs/sentiment/quarter_alt");
    for (key, present) in [("2026-Q2", q2_present), ("2026-Q3", q3_present)] {
        let values = attrs.join(key).join("values.arrow");
        let presence = attrs.join(key).join("presence.roaring");
        assert!(values.is_file(), "{key} has a column");
        let bytes = std::fs::read(&presence).expect("a presence bitmap beside it");
        let bitmap = croaring::Bitmap::try_deserialize::<croaring::Portable>(&bytes)
            .expect("the presence bitmap deserialises");
        assert_eq!(
            bitmap.cardinality(),
            present,
            "{key}'s presence is its own rows' values"
        );
        // Every file the build wrote is digested, which is what `tessera verify` walks.
        for path in [&values, &presence] {
            let rel = path
                .strip_prefix(out.join("v00000"))
                .unwrap()
                .to_string_lossy()
                .to_string();
            assert!(
                bundle.manifest.files.contains_key(&rel),
                "{rel} is in the manifest"
            );
        }
    }

    // **The family is recorded on its group, not among the declared scalars** (contracts §2.2):
    // `declared_scalars` is one flat bundle-wide list addressed positionally, and a column placed
    // there would take a slot in every row's tail and a whole-corpus `attrs/` directory of its
    // own — both absent for every entity. The record is what the engine opens the columns from
    // and what `/v1/meta` publishes the scope from, so its absence would leave the files unread.
    assert!(
        !bundle
            .manifest
            .declared_scalars
            .iter()
            .any(|d| d.name == "sentiment"),
        "a scoped column is not one of the declared scalars"
    );
    let families = bundle.manifest.scoped_scalars();
    assert_eq!(families.len(), 1, "one family");
    assert_eq!(families[0].name, "sentiment");
    assert_eq!(families[0].group, "quarter_alt");
    assert!(families[0].index);
    assert_eq!(
        families[0].views,
        vec![
            "quarter_alt:2026-Q2".to_string(),
            "quarter_alt:2026-Q3".to_string()
        ],
        "the views that have a column, in roster order"
    );
}

/// **A sparse view stores the pages it occupies, not the entity space it is bounded by**
/// (`views.md` §8; the paged `permutation.bin`, `tessera_store::permutation`).
///
/// The cost this test is about only exists above 2¹⁶ entities — a page covers that many
/// consecutive ids, so every other fixture in this repository fits in one page and would show a
/// saving of zero. So the entity space here is four pages wide and one view holds a slice of it,
/// which is the shape a group of quarterly views has at any real scale: forty views over one
/// entity space, each holding a fraction of it, each of them a full flat array today.
///
/// **The comparison is computed, not quoted.** The flat form is `16 + 4 × bound` for the same
/// bound — the header and one `u32` per entity id — and the assertion is against that number
/// rather than against a figure someone would have to re-derive when the bound moves.
#[test]
fn a_sparse_views_permutation_costs_its_pages_and_not_its_bound() {
    /// A page of entity space, which is the unit the file stores.
    const PAGE: u64 = 1 << 16;
    /// Four pages of entity space, so that a one-page view is visibly cheaper than the bound.
    const WIDE: u64 = 4 * PAGE;
    /// The sparse view's population — well under a page, and given a term of its own below so
    /// that the build's signature-sorted allocation keeps its entity ids together.
    const SPARSE: u64 = 5_000;

    let dir = tempfile::tempdir().unwrap();
    let world_points = dir.path().join("wide-world.parquet");
    let sparse_points = dir.path().join("wide-sparse.parquet");
    let pairs = dir.path().join("wide-pairs.parquet");
    write_points(&world_points, "world", 0..WIDE);
    write_points(&sparse_points, "quarter:2026-Q2", 0..SPARSE);

    // One term for the sparse view's members and another for everything else. Entity ids are
    // assigned in **term-signature** order (architecture §11.1), so this is what makes the view's
    // entities a contiguous block rather than a scatter across all four pages — and the scatter
    // is the case the paging cannot help, which the assertions below would catch.
    {
        let schema = Arc::new(Schema::new(vec![
            Field::new("entity_id", DataType::UInt64, false),
            Field::new("term_id", DataType::UInt32, false),
        ]));
        let entities: Vec<u64> = (0..WIDE).collect();
        let terms: Vec<u32> = entities
            .iter()
            .map(|&e| if e < SPARSE { 9 } else { 1 })
            .collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt64Array::from(entities)),
                Arc::new(UInt32Array::from(terms)),
            ],
        )
        .unwrap();
        let mut writer = ArrowWriter::try_new(File::create(&pairs).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    let out = dir.path().join("bundle");
    build(&BuildArgs {
        views: vec![
            view_args("world", &world_points, &pairs),
            view_args("quarter:2026-Q2", &sparse_points, &pairs),
        ],
        anchor: 0,
        groups: vec![tessera_store::manifest::GroupDescriptor {
            title: None,
            name: "quarter".to_string(),
            members_of: None,
            point_default: Some("public".to_string()),
            visibility: None,
            scoped_scalars: Vec::new(),
            quantisation: group_frame(),
            projection: tessera_spatial::Projection::None,
            metadata: Vec::new(),
            views: vec![tessera_store::manifest::GroupViewDescriptor {
                key: "2026-Q2".to_string(),
                visibility: None,
                metadata: Default::default(),
            }],
        }],
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
        out: out.clone(),
        limit: None,
        identity_key: IdentityKey::from_hex(TEST_KEY_HEX).unwrap(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: Default::default(),
    })
    .expect("a wide two-view build succeeds");

    let bundle = open_bundle(&out).expect("the bundle opens");
    let partition = bundle.partitions.get("default").expect("one partition");
    let sparse = partition
        .views
        .get("quarter:2026-Q2")
        .expect("the group's view");
    let base = sparse.row_space.base();
    assert_eq!(base.bound(), WIDE, "every view is bounded by entity space");
    assert_eq!(base.page_count(), 4);
    assert!(
        base.present_pages() <= 2,
        "5,000 entities of one signature occupy one page, or two when the block straddles a \
         boundary — {} pages of 4",
        base.present_pages()
    );

    let paged = std::fs::metadata(base.path())
        .expect("stat permutation.bin")
        .len();
    // What the flat array this replaced would have cost at the same bound: a 16-byte header and a
    // `u32` per entity id, present or absent.
    let flat = 16 + 4 * WIDE;
    assert!(
        paged * 2 < flat,
        "the sparse view's permutation is {paged} bytes against the flat form's {flat} — the \
         paging must at least halve it at four pages"
    );

    // The dense view over the same entity space keeps every page, which is the degenerate case:
    // it pays the directory and its padding and nothing else.
    let world = partition.views.get("world").expect("the plain view");
    assert_eq!(world.row_space.base().present_pages(), 4);
    let dense = std::fs::metadata(world.row_space.base().path())
        .expect("stat")
        .len();
    assert!(
        dense >= flat && dense < flat + 8192,
        "a dense view costs the flat form plus a directory: {dense} against {flat}"
    );
}
