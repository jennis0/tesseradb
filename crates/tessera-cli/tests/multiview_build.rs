//! **The whole of a multi-view declaration, built through the binary** (`views.md` §7).
//!
//! What this file is for is the half of the build the library's own tests cannot reach: turning a
//! `corpus.toml` into build arguments — the view registry over a plain view and two groups, the
//! anchor, one frame per group, a scoped attribute's column family, and the layers' `views`
//! expanded from a group name to the views of it. Every assertion below is against the bundle that
//! comes out, or against what the build said while writing it.

use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, Float32Array, Float64Array, ListBuilder, StringArray, StringBuilder, UInt64Array,
    UInt64Builder,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use tessera_store::read::open_bundle;
use tessera_types::EntityId;

fn tessera() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tessera"))
}

const ENTITIES: u64 = 30;
/// `world` holds the first twenty; the group's two views overlap in the middle, so an entity is in
/// one view, in both, or in neither — the shape `views.md` §4's join rule is about.
const WORLD: std::ops::Range<u64> = 0..20;
const Q1: std::ops::Range<u64> = 0..15;
const Q2: std::ops::Range<u64> = 10..30;

/// A view's own layout: the same entity sits somewhere different in each.
fn position(view: &str, e: u64) -> (f64, f64) {
    match view {
        "world" => ((e % 5) as f64 * 10.0, (e / 5) as f64 * 10.0),
        "q1" => (90.0 - (e % 5) as f64 * 10.0, (e / 5) as f64 * 7.0),
        _ => ((e % 7) as f64 * 9.0, 90.0 - (e / 7) as f64 * 6.0),
    }
}

/// A points file with no discriminator: `entity_id, x, y, access`, plus `sentiment` where the
/// group's scoped attribute is read from it.
fn write_points(path: &Path, view: &str, ids: std::ops::Range<u64>, sentiment: bool) {
    let mut fields = vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("access", DataType::Utf8, true),
    ];
    if sentiment {
        fields.push(Field::new("sentiment", DataType::Float32, true));
    }
    let schema = Arc::new(Schema::new(fields));
    let ids: Vec<u64> = ids.collect();
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(ids.clone())),
        Arc::new(Float64Array::from(
            ids.iter().map(|&e| position(view, e).0).collect::<Vec<_>>(),
        )),
        Arc::new(Float64Array::from(
            ids.iter().map(|&e| position(view, e).1).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            ids.iter()
                .map(|&e| if e % 4 == 0 { None } else { Some("public") })
                .collect::<Vec<_>>(),
        )),
    ];
    if sentiment {
        columns.push(Arc::new(Float32Array::from(
            ids.iter()
                .map(|&e| (e % 3 != 0).then_some(e as f32 / 100.0))
                .collect::<Vec<_>>(),
        )));
    }
    write(path, schema, columns);
}

/// Form B: one file, a `quarter` column saying which view each row lands in.
fn write_discriminated(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("quarter", DataType::Utf8, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("access", DataType::Utf8, true),
    ]));
    let (mut ids, mut keys, mut xs, mut ys, mut access) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (key, range) in [("q1", Q1), ("q2", Q2)] {
        for e in range {
            // A *different* layout from the owning group's, over the same membership — which is
            // what a `members` group is (`views.md` §3.3).
            ids.push(e);
            keys.push(key.to_string());
            xs.push(position(key, e).1);
            ys.push(position(key, e).0);
            access.push(if e % 4 == 0 { None } else { Some("public") });
        }
    }
    write(
        path,
        schema,
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(StringArray::from(keys)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(access)),
        ],
    );
}

/// One artifact per row: `key`, its members, one content value, and — on the scoped layer — the
/// view it belongs to.
fn write_artifacts(path: &Path, rows: &[(&str, Option<&str>, Vec<u64>)]) {
    let scoped = rows.iter().any(|(_, view, _)| view.is_some());
    let mut fields = vec![Field::new("key", DataType::Utf8, false)];
    if scoped {
        fields.push(Field::new("quarter", DataType::Utf8, false));
    }
    fields.push(Field::new(
        "members",
        DataType::List(Arc::new(Field::new("item", DataType::UInt64, true))),
        false,
    ));
    fields.push(Field::new(
        "contents",
        DataType::List(Arc::new(Field::new(
            "item",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ))),
        false,
    ));
    let schema = Arc::new(Schema::new(fields));
    let mut members = ListBuilder::new(UInt64Builder::new());
    let mut contents = ListBuilder::new(ListBuilder::new(StringBuilder::new()));
    for (_, _, ids) in rows {
        for id in ids {
            members.values().append_value(*id);
        }
        members.append(true);
        contents.values().values().append_value("tag");
        contents.values().append(true);
        contents.append(true);
    }
    let mut columns: Vec<ArrayRef> = vec![Arc::new(StringArray::from(
        rows.iter().map(|(key, _, _)| *key).collect::<Vec<_>>(),
    ))];
    if scoped {
        columns.push(Arc::new(StringArray::from(
            rows.iter()
                .map(|(_, view, _)| view.expect("a scoped layer's rows all name a view"))
                .collect::<Vec<_>>(),
        )));
    }
    columns.push(Arc::new(members.finish()));
    columns.push(Arc::new(contents.finish()));
    write(path, schema, columns);
}

fn write(path: &Path, schema: Arc<Schema>, columns: Vec<ArrayRef>) {
    let batch = RecordBatch::try_new(schema.clone(), columns).expect("a well-formed batch");
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

const CORPUS: &str = r#"
[sources]
world    = "world.parquet"
q1       = "q1.parquet"
q2       = "q2.parquet"
alt      = "alt.parquet"
sets     = "sets.parquet"
clusters = "clusters.parquet"

[defaults]
allocation_view = "world"

[[view]]
name             = "world"
source           = "world"
extent           = { x = [0.0, 100.0], y = [0.0, 100.0] }
point_visibility = { field = "access", default = "public" }

[[view_group]]
name             = "quarter"
extent           = "auto"
point_visibility = { field = "access", default = "public" }
metadata         = { label = "text" }

[[view_group.view]]
key    = "q1"
source = "q1"
label  = "Q1"

[[view_group.view]]
key    = "q2"
source = "q2"
label  = "Q2"

[[view_group]]
name             = "quarter_alt"
members          = "quarter"
extent           = { x = [0.0, 100.0], y = [0.0, 100.0] }
source           = "alt"
fields           = { view = "quarter" }
point_visibility = { field = "access", default = "public" }

[[attribute]]
name   = "sentiment"
type   = "f32"
scope  = { group = "quarter" }
index  = true

[[layer]]
name                      = "collections"
views                     = ["world", "quarter"]
source                    = "sets"
membership                = "enumerated"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

[layer.content]
computed = []

[[layer.content.supplied]]
name                      = "tag"
type                      = "text"
require_member_visibility = "inherited"

[[layer]]
name                      = "clusters"
views                     = ["quarter"]
scope                     = { group = "quarter" }
source                    = "clusters"
fields                    = { view = "quarter" }
membership                = "enumerated"
hierarchy                 = { kind = "flat" }
visibility                = "public"
artifact_visibility       = { default = "inherited" }
require_member_visibility = "any"

[layer.content]
computed = []

[[layer.content.supplied]]
name                      = "tag"
type                      = "text"
require_member_visibility = "inherited"
"#;

const DEPLOYMENT: &str = r#"
[bundle]
path  = "bundle"
cache = ".tessera/cache"
wal   = ".tessera/wal.log"

[build]
schema = "corpus.toml"

[plugin]
module = "builtin:passthrough"

[identity]
env = "TESSERA_TEST_IDENTITY_KEY"

[disclosure]
min_visible_members = 1
token_max_lifetime  = 3600

[serve]
viewer  = "127.0.0.1:18081"
session = "127.0.0.1:18082"
control = "127.0.0.1:18083"
max_k   = 200
"#;

/// **One declaration, nine — here five — row spaces over one entity space**, and every shape
/// `views.md` adds to a build: form A and form B rosters over one key set, a group-scoped
/// attribute's column family, a layer naming a group, and a layer scoped to one.
#[test]
fn the_whole_declaration_builds_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let at = |name: &str| dir.path().join(name);
    write_points(&at("world.parquet"), "world", WORLD, false);
    write_points(&at("q1.parquet"), "q1", Q1, true);
    write_points(&at("q2.parquet"), "q2", Q2, true);
    write_discriminated(&at("alt.parquet"));
    write_artifacts(
        &at("sets.parquet"),
        &[
            // Both sets have members in every view the layer is drawn on: one artifact set,
            // drawn on all of them, is what an unscoped layer is (`views.md` §3.5).
            ("set-a", None, (0..12).collect()),
            ("set-b", None, (8..25).collect()),
        ],
    );
    // Two artifacts per view, and the keys of one view are not the other's.
    write_artifacts(
        &at("clusters.parquet"),
        &[
            ("q1-lo", Some("q1"), (0..8).collect()),
            ("q1-hi", Some("q1"), (8..15).collect()),
            ("q2-lo", Some("q2"), (10..20).collect()),
            ("q2-hi", Some("q2"), (20..30).collect()),
        ],
    );
    std::fs::write(at("corpus.toml"), CORPUS).unwrap();
    std::fs::write(at("tessera.toml"), DEPLOYMENT).unwrap();

    let built = tessera()
        .current_dir(dir.path())
        .args(["build", "--mint-id-key"])
        .env("TESSERA_TEST_IDENTITY_KEY", "")
        .output()
        .expect("the build runs");
    let stderr = String::from_utf8_lossy(&built.stderr).to_string();
    let stdout = String::from_utf8_lossy(&built.stdout).to_string();
    assert!(built.status.success(), "{stderr}");

    // **One row space per view, each its own population** (`views.md` §8).
    let rows_of = |range: std::ops::Range<u64>| range.end - range.start;
    for (view, rows) in [
        ("world", rows_of(WORLD)),
        ("quarter:q1", rows_of(Q1)),
        ("quarter:q2", rows_of(Q2)),
        ("quarter_alt:q1", rows_of(Q1)),
        ("quarter_alt:q2", rows_of(Q2)),
    ] {
        assert!(
            stdout.contains(&format!("view {view}: {rows} row(s)")),
            "{view} is {rows} rows\n{stdout}"
        );
    }
    // One frame for the group, fitted over both its views' sources (`views.md` §3.1).
    assert!(
        stderr.contains("view group 'quarter': quantising against"),
        "{stderr}"
    );

    // **The unscoped layer is drawn on every view it names** — the plain view and both views of
    // the group its `views` names — with the one artifact set in each.
    for view in ["world", "quarter:q1", "quarter:q2"] {
        assert!(
            stderr.contains(&format!("collections level 0 [{view}]: 2 artifact(s) with rows")),
            "collections is drawn on {view}\n{stderr}"
        );
    }
    // **The scoped layer is a different artifact set per view**: two of its four artifacts have
    // rows in each view, and they are that view's own (`views.md` §3.5).
    for view in ["quarter:q1", "quarter:q2"] {
        assert!(
            stderr.contains(&format!("clusters level 0 [{view}]: 2 artifact(s) with rows")),
            "clusters resolves per view at {view}\n{stderr}"
        );
    }
    assert!(
        !stderr.contains("clusters level 0 [quarter_alt"),
        "a layer is drawn only in the views it names\n{stderr}"
    );

    // **The column family**: one column per view of the group, under `attrs/<column>/<group>/<key>`
    // (`views.md` §5).
    let bundle = at("bundle");
    for key in ["q1", "q2"] {
        for file in ["values.arrow", "presence.roaring"] {
            let rel = format!("partitions/default/attrs/sentiment/quarter/{key}/{file}");
            assert!(bundle.join("v00000").join(&rel).is_file(), "{rel}");
        }
    }

    // **One entity space under two layouts**: `quarter_alt:q2` holds exactly the entities
    // `quarter:q2` does, in a different geometry (`views.md` §3.3).
    let opened = open_bundle(&bundle).expect("the bundle opens");
    assert_eq!(opened.manifest.views.len(), 5);
    let partition = opened.partitions.get("default").expect("one partition");
    for key in ["q1", "q2"] {
        let owner = partition
            .views
            .get(&format!("quarter:{key}"))
            .expect("the owning group's view");
        let sharing = partition
            .views
            .get(&format!("quarter_alt:{key}"))
            .expect("the sharing group's view");
        let held = |view: &tessera_store::read::ViewData| -> Vec<u64> {
            (0..ENTITIES)
                .filter(|&e| view.row_space.row_of(EntityId::new(e)).is_some())
                .collect()
        };
        assert_eq!(held(owner), held(sharing), "same membership at {key}");
        let (owner_codes, sharing_codes) = (
            owner.segments[0].morton.u32(),
            sharing.segments[0].morton.u32(),
        );
        assert!(
            owner_codes != sharing_codes,
            "two layouts over one key set are two geometries"
        );
    }

    // The bundle verifies, shallow and deep — every file the families and the per-view artifact
    // structures added is digested.
    for args in [vec!["verify"], vec!["verify", "--deep"]] {
        let mut command = tessera();
        command.current_dir(dir.path()).args(&args).arg("bundle");
        let out = command.output().expect("verify runs");
        assert!(
            out.status.success(),
            "{:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
