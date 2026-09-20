//! **A group with no roster at all builds, its keys minted from the discriminator's distinct
//! values** (`views.md` §3.1's third form, §7; owner ruling 2026-08-31).
//!
//! The mint is the only thing here the other multi-view tests do not already cover, and what it
//! has to get right is that it produces *ordinary roster records*: everything below the mint — the
//! registry, the manifest's group descriptor, each view's row space — must be unable to tell a
//! minted roster from a written one. So the assertions are the ones `multiview_build.rs` makes of
//! a declared roster, against a declaration that names no keys.
//!
//! The one property that is the mint's own is **order**: the keys are sorted by key bytes, not by
//! the order the source's rows happen to arrive in, because roster order is served order
//! (decision 0113) and a rebuild must serve one corpus's views in one order.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use tessera_store::read::open_bundle;

fn tessera() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_tessera"))
}

const ENTITIES: u64 = 24;
/// The three key sets, and the rows the group's own file carries for each. They overlap, which is
/// the ordinary case: an entity is in one view, in several, or in none.
const POPULATION: [(&str, std::ops::Range<u64>); 3] =
    [("2026-Q3", 0..12), ("2026-Q1", 6..18), ("2026-Q2", 12..24)];

fn position(view: &str, e: u64) -> (f64, f64) {
    match view {
        "world" => ((e % 6) as f64 * 10.0, (e / 6) as f64 * 10.0),
        "2026-Q1" => (90.0 - (e % 6) as f64 * 10.0, (e / 6) as f64 * 7.0),
        "2026-Q2" => ((e % 4) as f64 * 9.0, 90.0 - (e / 4) as f64 * 6.0),
        _ => ((e % 3) as f64 * 8.0, (e / 3) as f64 * 5.0),
    }
}

fn write(path: &Path, schema: Arc<Schema>, columns: Vec<ArrayRef>) {
    let batch = RecordBatch::try_new(schema.clone(), columns).expect("a well-formed batch");
    let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
}

/// The plain view's points: the whole entity space, and the anchor the ids are ordered against.
fn write_world(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let ids: Vec<u64> = (0..ENTITIES).collect();
    write(
        path,
        schema,
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| position("world", e).0).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter().map(|&e| position("world", e).1).collect::<Vec<_>>(),
            )),
        ],
    );
}

/// The group's points, one file behind a `quarter` column and **no roster anywhere**. The keys
/// arrive in `POPULATION`'s order — `2026-Q3` first — so file order is not sorted order.
fn write_discriminated(path: &Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("quarter", DataType::Utf8, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    let (mut ids, mut keys, mut xs, mut ys) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for (key, range) in POPULATION {
        for e in range {
            ids.push(e);
            keys.push(key.to_string());
            xs.push(position(key, e).0);
            ys.push(position(key, e).1);
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
        ],
    );
}

const CORPUS: &str = r#"
[sources]
world   = "world.parquet"
quarter = "quarter.parquet"

[defaults]
allocation_view = "world"

[[view]]
name             = "world"
source           = "world"
extent           = { x = [0.0, 100.0], y = [0.0, 100.0] }
point_visibility = { default = "public" }

[[view_group]]
name             = "quarter"
extent           = { x = [0.0, 100.0], y = [0.0, 100.0] }
source           = "quarter"
fields           = { view = "quarter" }
point_visibility = { default = "public" }
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
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:18091"
session = "127.0.0.1:18092"
control = "127.0.0.1:18093"
max_k   = 200
"#;

/// Write `corpus` and a deployment into a fresh directory, with both points files beside them.
fn fixture(corpus: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_world(&dir.path().join("world.parquet"));
    write_discriminated(&dir.path().join("quarter.parquet"));
    std::fs::write(dir.path().join("corpus.toml"), corpus).unwrap();
    std::fs::write(dir.path().join("tessera.toml"), DEPLOYMENT).unwrap();
    dir
}

fn build_in(dir: &Path) -> std::process::Output {
    tessera()
        .current_dir(dir)
        .args(["build", "--mint-id-key"])
        .env("TESSERA_TEST_IDENTITY_KEY", "")
        .output()
        .expect("the build runs")
}

#[test]
fn a_group_with_no_roster_builds_its_minted_views() {
    let dir = fixture(CORPUS);
    let built = build_in(dir.path());
    let stderr = String::from_utf8_lossy(&built.stderr).to_string();
    let stdout = String::from_utf8_lossy(&built.stdout).to_string();
    assert!(built.status.success(), "{stderr}");

    // **One row space per minted view, each its own population** — the discriminator selects the
    // rows exactly as it does under a written roster.
    for (key, range) in POPULATION {
        let rows = range.end - range.start;
        assert!(
            stdout.contains(&format!("view quarter:{key}: {rows} row(s)")),
            "quarter:{key} is {rows} rows\n{stdout}"
        );
    }

    let opened = open_bundle(&dir.path().join("bundle")).expect("the bundle opens");
    // **The served roster is the mint, in key-byte order** (decision 0113: roster order is served
    // order), and not the order the keys appear in the file.
    let group = opened
        .manifest
        .groups
        .iter()
        .find(|g| g.name == "quarter")
        .expect("the minted group is published");
    assert_eq!(
        group.views.iter().map(|v| v.key.as_str()).collect::<Vec<_>>(),
        ["2026-Q1", "2026-Q2", "2026-Q3"]
    );
    // A minted record is an ordinary roster record carrying nothing of its own: no metadata to
    // sit on it, and the group's own gate.
    assert!(group.metadata.is_empty(), "no metadata is declarable");
    for view in &group.views {
        assert!(view.metadata.is_empty(), "{}", view.key);
        assert_eq!(view.visibility, None, "{}", view.key);
    }
    assert_eq!(opened.manifest.views.len(), 4, "one plain view and three minted");

    // Each minted view holds its own entities and nothing else.
    let partition = opened.partitions.get("default").expect("one partition");
    for (key, range) in POPULATION {
        let view = partition
            .views
            .get(&format!("quarter:{key}"))
            .expect("a minted view is in the bundle");
        let rows = (0..ENTITIES)
            .filter(|&e| {
                view.row_space
                    .row_of(tessera_types::EntityId::new(e))
                    .is_some()
            })
            .count() as u64;
        assert_eq!(rows, range.end - range.start, "quarter:{key}");
    }

    // The bundle verifies: every file the minted views wrote is digested like any other's.
    let out = tessera()
        .current_dir(dir.path())
        .args(["verify", "--deep", "bundle"])
        .output()
        .expect("verify runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// **A distinct value that cannot be a key is a refusal naming the value and the column** — not a
/// skipped value whose rows would then belong to no view (`views.md` §3.1, §3.2).
#[test]
fn a_minted_key_outside_the_charset_refuses_the_build() {
    let dir = tempfile::tempdir().unwrap();
    write_world(&dir.path().join("world.parquet"));
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("quarter", DataType::Utf8, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
    ]));
    write(
        &dir.path().join("quarter.parquet"),
        schema,
        vec![
            Arc::new(UInt64Array::from(vec![0u64, 1])),
            Arc::new(StringArray::from(vec!["2026-Q1", "2026 Q2"])),
            Arc::new(Float64Array::from(vec![1.0, 2.0])),
            Arc::new(Float64Array::from(vec![1.0, 2.0])),
        ],
    );
    std::fs::write(dir.path().join("corpus.toml"), CORPUS).unwrap();
    std::fs::write(dir.path().join("tessera.toml"), DEPLOYMENT).unwrap();

    let built = build_in(dir.path());
    let stderr = String::from_utf8_lossy(&built.stderr).to_string();
    assert!(!built.status.success(), "the build must refuse\n{stderr}");
    assert!(stderr.contains("'2026 Q2'"), "{stderr}");
    assert!(stderr.contains("column 'quarter'"), "{stderr}");
}
