//! **Unique fields at a running service**: `eq` and `in` on a unique column answer from its index
//! and equal the items that hold the values, before and after a flush, a coalesce, a fold and a
//! restart; a holder the viewer cannot see answers as absent; a row giving an item a value
//! another item holds names both and is refused, and one naming nothing else names the holder and
//! edits it; `unique` declared on a column that exists is built over its values and refused where
//! two items hold one value; removing it keeps the values.
//!
//! The fixture declares three unique columns at the build: `doi`, a keyword with no other home
//! than the record blob, so `eq` and `in` are the only filters it takes; `gid`, an indexed `u64`
//! whose values pass 2^53; and `serial`, a rendered `i64` holding negative values. Beside them it
//! declares `id`, the points file's `entity_id`, which no row here changes, so a test can find
//! the item the build made of each source row.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{Float64Array, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use mosaica_engine::filter::{FilterExpr, FilterOperand, Scalar};
use mosaica_engine::{AcceptError, AttributeRequest, Engine, EngineConfig, Session, ViewportRequest};
use mosaica_lifecycle::command::UnallocatedRow;
use mosaica_lifecycle::wal::WalScalar;
use mosaica_lifecycle::{ChangeOp, ExecError};
use mosaica_types::layer::LayerScope;
use mosaica_types::EntityId;

const N: u64 = 90;
const VIEWPORT: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
const BIG: u64 = 1 << 60;

const SCHEMA_TOML: &str = r#"
[[attribute]]
name   = "doi"
type   = "keyword"
unique = true

[[attribute]]
name   = "gid"
type   = "u64"
index  = true
unique = true

[[attribute]]
name   = "serial"
type   = "i64"
render = true
unique = true

[[attribute]]
name = "note"
type = "keyword"
"#;

fn doi_of(source: u64) -> String {
    format!("10.{source}/built")
}

fn gid_of(source: u64) -> u64 {
    BIG + source * 7
}

fn serial_of(source: u64) -> i64 {
    -(source as i64) - 5
}

/// A note shared by every third item, so a runtime declaration over it meets duplicates.
fn note_of(source: u64) -> String {
    format!("n{}", source / 3)
}

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("doi", DataType::Utf8, false),
        Field::new("gid", DataType::UInt64, false),
        Field::new("serial", DataType::Int64, false),
        Field::new("note", DataType::Utf8, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter().map(|e| ((e * 37) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 53) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| doi_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(UInt64Array::from(
                ids.iter().map(|e| gid_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                ids.iter().map(|e| serial_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| note_of(*e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

struct Fixture {
    tmp: tempfile::TempDir,
    root: std::path::PathBuf,
}

/// The fixture, with `unique` taken off every column where `unique` is false, so a runtime
/// declaration can be compared with a build one over the same values.
fn fixture_with(unique: bool) -> Fixture {
    fixture_of(unique, &["s0"])
}

/// The fixture over `views`, each holding every built item, so an item ingested into one can
/// join another.
fn fixture_of(unique: bool, views: &[&str]) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points(&points, N);
    write_pairs_n(&pairs, N);
    let schema_path = tmp.path().join("schema.toml");
    let toml = match unique {
        true => SCHEMA_TOML.to_string(),
        false => SCHEMA_TOML.replace("unique = true\n", ""),
    };
    std::fs::write(&schema_path, toml).unwrap();
    let schema = mosaica_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the fixture schema parses")
        .schema;
    let schema = with_id(schema);
    mosaica_build::build(&mosaica_build::BuildArgs {
        views: views
            .iter()
            .map(|view| mosaica_build::ViewArgs {
                visibility: None,
                view_id: view.to_string(),
                projection: mosaica_spatial::Projection::None,
                extent: extent(),
                points: points.clone(),
                point_fields: Default::default(),
                select: None,
                access: mosaica_build::config::AccessInput::relation(pairs.clone()),
            })
            .collect(),
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(points, &schema),
        out: root.clone(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: true,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema,
    })
    .expect("the build succeeds");
    Fixture { tmp, root }
}

fn fixture() -> Fixture {
    fixture_with(true)
}

fn engine_with(fx: &Fixture, config: EngineConfig) -> Engine {
    let mut engine = Engine::open(
        &fx.root,
        &fx.tmp.path().join("cache"),
        &fx.tmp.path().join("wal.log"),
        config,
    )
    .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

fn engine_over(fx: &Fixture) -> Engine {
    engine_with(
        fx,
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            ..config_uncapped()
        },
    )
}

fn restart(fx: &Fixture, engine: Engine) -> Engine {
    drop(engine);
    engine_over(fx)
}

/// A row carrying the four declared columns, under `label`.
fn row(engine: &Engine, id: &str, label: &[u8], doi: &str, gid: u64, serial: i64) -> UnallocatedRow {
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![label.to_vec()],
        x: 500.0,
        y: 500.0,
        scalars: vec![
            WalScalar::Utf8(doi.to_string()),
            WalScalar::U64(gid),
            WalScalar::I64(serial),
            WalScalar::Utf8(format!("note-{id}")),
        ],
        terms: engine.resolve_terms(&[label.to_vec()]),
        scoped: Vec::new(),
    }
}

fn hash_of(batch: &str) -> [u8; 32] {
    let mut hash = [0u8; 32];
    let n = batch.len().min(32);
    hash[..n].copy_from_slice(&batch.as_bytes()[..n]);
    hash
}

fn try_ingest(
    engine: &Engine,
    batch: &str,
    rows: Vec<UnallocatedRow>,
) -> Result<Vec<EntityId>, AcceptError> {
    engine.ingest_rows(rows, batch.to_string(), hash_of(batch))
}

fn ingest(engine: &Engine, batch: &str, rows: Vec<UnallocatedRow>) -> Vec<EntityId> {
    try_ingest(engine, batch, rows).unwrap_or_else(|e| panic!("batch {batch} is accepted: {e}"))
}

fn is_taken(result: Result<Vec<EntityId>, AcceptError>) -> bool {
    taken(&result)
}

/// A row naming one item and carrying a value another item holds names both, so the batch is
/// refused.
fn taken(result: &Result<Vec<EntityId>, AcceptError>) -> bool {
    matches!(result, Err(AcceptError::Conflict(_)))
}

/// `r`, naming the built item of `source` by its `tessera_id` beside whatever its values name.
fn naming_built(engine: &Engine, mut r: UnallocatedRow, source: u64) -> UnallocatedRow {
    r.join = Some(built(engine, source));
    r
}

/// The item the build made of `source`, found by the `id` no row here changes.
fn built(engine: &Engine, source: u64) -> EntityId {
    item_of_id(engine, source).unwrap().expect("the built item holds its id")
}

fn full(engine: &Engine) -> Session {
    engine.authorise(&full_coverage_credential()).unwrap()
}

/// The `tessera_id`s a viewport of `s0` under `filter` returns.
fn matching(engine: &Engine, session: &Session, filter: FilterExpr) -> BTreeSet<u64> {
    matching_in(engine, session, "s0", filter)
}

/// The `tessera_id`s a viewport of `view` under `filter` returns.
fn matching_in(engine: &Engine, session: &Session, view: &str, filter: FilterExpr) -> BTreeSet<u64> {
    let mut req = ViewportRequest::new(view, 0, VIEWPORT, 10_000);
    req.filter = Some(filter);
    engine
        .viewport(session, req)
        .unwrap()
        .points
        .iter()
        .map(|(id, _)| id.raw())
        .collect()
}

fn text_in(column: &str, values: &[String]) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.to_string(),
        operand: FilterOperand::TextIn(values.to_vec()),
    }
}

fn number_in(column: &str, values: &[i128]) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.to_string(),
        operand: FilterOperand::NumIn(values.iter().map(|v| Scalar::Int(*v)).collect()),
    }
}

/// Every item's three unique values and its `tessera_id`: the build's, from its source ids,
/// then whatever a case ingests.
#[derive(Default)]
struct Expected {
    by_doi: BTreeMap<String, u64>,
    by_gid: BTreeMap<u64, u64>,
    by_serial: BTreeMap<i64, u64>,
}

impl Expected {
    fn built(fx: &Fixture, engine: &Engine) -> Expected {
        let mut expected = Expected::default();
        for (source, entity) in source_to_new_map(&fx.root, "v00000") {
            let id = engine.tessera_id_of(EntityId::new(entity)).unwrap().raw();
            expected.add(id, &doi_of(source), gid_of(source), serial_of(source));
        }
        expected
    }

    fn add(&mut self, id: u64, doi: &str, gid: u64, serial: i64) {
        self.by_doi.insert(doi.to_string(), id);
        self.by_gid.insert(gid, id);
        self.by_serial.insert(serial, id);
    }

    fn remove(&mut self, id: u64) {
        self.by_doi.retain(|_, held| *held != id);
        self.by_gid.retain(|_, held| *held != id);
        self.by_serial.retain(|_, held| *held != id);
    }
}

/// Every value `in` a sample of the expected ones plus three nobody holds, on each column,
/// answers exactly the items holding them.
fn check_lookups(engine: &Engine, expected: &Expected, when: &str) {
    let session = full(engine);
    let dois: Vec<String> = expected
        .by_doi
        .keys()
        .step_by(4)
        .cloned()
        .chain(["10.nobody/x".to_string()])
        .collect();
    let want: BTreeSet<u64> = dois.iter().filter_map(|d| expected.by_doi.get(d)).copied().collect();
    assert_eq!(matching(engine, &session, text_in("doi", &dois)), want, "doi, {when}");

    let gids: Vec<i128> = expected
        .by_gid
        .keys()
        .step_by(3)
        .map(|g| *g as i128)
        .chain([1, BIG as i128 + 1])
        .collect();
    let want: BTreeSet<u64> = gids
        .iter()
        .filter_map(|g| expected.by_gid.get(&(*g as u64)))
        .copied()
        .collect();
    assert_eq!(matching(engine, &session, number_in("gid", &gids)), want, "gid, {when}");

    let serials: Vec<i128> = expected
        .by_serial
        .keys()
        .step_by(5)
        .map(|s| *s as i128)
        .chain([7, i128::from(i64::MAX) + 1])
        .collect();
    let want: BTreeSet<u64> = serials
        .iter()
        .filter_map(|s| i64::try_from(*s).ok())
        .filter_map(|s| expected.by_serial.get(&s))
        .copied()
        .collect();
    assert_eq!(
        matching(engine, &session, number_in("serial", &serials)),
        want,
        "serial, {when}"
    );
}

// ---------------------------------------------------------------------------------------------

/// **`eq` and `in` on a unique column answer exactly the items holding the values**, from the
/// build's index, from live entries before a flush, from the runs flushes write, from the run a
/// coalesce merges them into, from the base a fold writes with a deleted item's entries dropped,
/// and after a restart replays the log over all of it.
#[test]
fn lookups_answer_the_holders_through_flush_coalesce_fold_and_restart() {
    let fx = fixture();
    let engine = engine_with(
        &fx,
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            coalesce_width: Some(2),
            ..config_uncapped()
        },
    );
    engine.set_merge_for_test(false);
    engine.set_coalesce_for_test(false);
    let mut expected = Expected::built(&fx, &engine);
    check_lookups(&engine, &expected, "as built");

    let add = |engine: &Engine, batch: &str, count: u64, expected: &mut Expected| {
        let rows: Vec<UnallocatedRow> = (0..count)
            .map(|i| {
                let id = format!("{batch}-{i}");
                let doi = format!("10.{batch}/{i}");
                let base = u64::from(batch.as_bytes()[0]) * 1000;
                row(engine, &id, b"0", &doi, BIG * 2 + base + i, -1_000_000 - (base + i) as i64 * 7)
            })
            .collect();
        let values: Vec<(String, u64, i64)> = rows
            .iter()
            .map(|r| match (&r.scalars[0], &r.scalars[1], &r.scalars[2]) {
                (WalScalar::Utf8(d), WalScalar::U64(g), WalScalar::I64(s)) => (d.clone(), *g, *s),
                _ => unreachable!(),
            })
            .collect();
        let entities = ingest(engine, batch, rows);
        for (entity, (doi, gid, serial)) in entities.iter().zip(values) {
            let id = engine.tessera_id_of(*entity).unwrap().raw();
            expected.add(id, &doi, gid, serial);
        }
        entities
    };

    // A buffered row has no position until its flush, so a lookup through the map meets it then.
    let first = add(&engine, "a", 3, &mut expected);
    publish_buffered(&engine);
    check_lookups(&engine, &expected, "after one flush");
    add(&engine, "b", 4, &mut expected);
    publish_buffered(&engine);
    add(&engine, "c", 2, &mut expected);
    publish_buffered(&engine);

    let before = engine.write_executor_stats();
    engine.set_coalesce_for_test(true);
    tick_until(&engine, "a coalesce", std::time::Duration::from_secs(60), || {
        let now = engine.write_executor_stats();
        now.coalesces > before.coalesces
    });
    engine.set_coalesce_for_test(false);
    assert_eq!(engine.write_executor_stats().coalesce_failures, 0);
    check_lookups(&engine, &expected, "after a coalesce");

    // A deleted item names nothing, before and after the fold drops its entries.
    let deleted = first[0];
    engine.accept_change(deleted, ChangeOp::Delete).unwrap();
    expected.remove(engine.tessera_id_of(deleted).unwrap().raw());
    check_lookups(&engine, &expected, "after a delete");
    fold(&engine);
    check_lookups(&engine, &expected, "after a fold");

    let engine = restart(&fx, engine);
    check_lookups(&engine, &expected, "after a restart");
    add(&engine, "d", 2, &mut expected);
    let engine = restart(&fx, engine);
    // The rows the log held are live entries again: their values are held.
    let doi = format!("10.d/{}", 0);
    assert!(is_taken(try_ingest(
        &engine,
        "after-restart",
        vec![naming_built(
            &engine,
            row(&engine, "r", b"0", &doi, BIG * 13, 7_777),
            1
        )]
    )));
    publish_buffered(&engine);
    check_lookups(&engine, &expected, "after the restart's flush");
}

/// **A generation taken before a coalesce and a fold still answers from its runs.** The fold
/// deletes the superseded prefix once nothing reads it, and a run the coalesce replaced is named
/// only by generations from before the coalesce. One of those opening the run for the first time
/// after the fold must still find it; once it is released, the prefix is deleted.
#[test]
fn a_generation_from_before_a_coalesce_and_a_fold_still_answers_from_its_runs() {
    let fx = fixture();
    let engine = engine_with(
        &fx,
        EngineConfig {
            flush_max_age_secs: 3600,
            flush_max_items: usize::MAX,
            coalesce_width: Some(2),
            ..config_uncapped()
        },
    );
    engine.set_merge_for_test(false);
    engine.set_coalesce_for_test(false);
    let a = ingest(&engine, "held-a", vec![row(&engine, "a", b"0", "10.held/a", BIG * 3, -3)]);
    publish_buffered(&engine);
    let b = ingest(&engine, "held-b", vec![row(&engine, "b", b"0", "10.held/b", BIG * 3 + 1, -4)]);
    publish_buffered(&engine);
    // Nothing has read the run the second flush wrote.
    let held = engine.generation();

    let before = engine.write_executor_stats();
    engine.set_coalesce_for_test(true);
    tick_until(&engine, "a coalesce", Duration::from_secs(60), || {
        engine.write_executor_stats().coalesces > before.coalesces
    });
    engine.set_coalesce_for_test(false);
    fold(&engine);

    let values = ["10.held/a".to_string(), "10.held/b".to_string()];
    let table = mosaica_engine::AddressTable {
        rows: values.len(),
        tessera_id: None,
        columns: vec![(
            "doi".to_string(),
            values
                .iter()
                .map(|v| Some(mosaica_engine::AddressValue::Text(v.clone())))
                .collect(),
        )],
    };
    let found = engine
        .name_items_under_for_test(&held, &table)
        .expect("the held generation reads its runs")
        .verdicts;
    assert_eq!(
        found,
        vec![
            mosaica_lifecycle::resolve::Verdict::Names(a[0]),
            mosaica_lifecycle::resolve::Verdict::Names(b[0])
        ]
    );

    let superseded = fx.root.join(&held.prefix);
    drop(held);
    tick_until(&engine, "the superseded prefix is deleted", Duration::from_secs(60), || {
        !superseded.exists()
    });
}

/// **A holder the viewer cannot see answers exactly as absent.** The restricted principal holds
/// only the subset term; an item carrying the other term alone is invisible to it, and asking for
/// that item's value answers what asking for a value nobody holds answers.
#[test]
fn an_invisible_holder_answers_as_absent() {
    let fx = fixture();
    let engine = engine_over(&fx);
    let restricted = engine.authorise(&subset_credential()).unwrap();
    let map = source_to_new_map(&fx.root, "v00000");
    let (hidden, shown) = (
        (0..N).find(|s| !subset_sees(*s)).unwrap(),
        (0..N).find(|s| subset_sees(*s)).unwrap(),
    );
    let shown_id = engine.tessera_id_of(EntityId::new(map[&shown])).unwrap().raw();
    let nobody = matching(&engine, &restricted, text_in("doi", &["10.nobody/x".to_string()]));
    assert!(nobody.is_empty());
    assert_eq!(
        matching(&engine, &restricted, text_in("doi", &[doi_of(hidden)])),
        nobody,
        "an invisible holder's value answers as a value nobody holds"
    );
    assert_eq!(
        matching(&engine, &restricted, text_in("doi", &[doi_of(hidden), doi_of(shown)])),
        BTreeSet::from([shown_id])
    );
    assert_eq!(
        matching(&engine, &restricted, number_in("gid", &[gid_of(hidden) as i128])),
        nobody
    );
}

/// **A cell of a type its unique field does not hold refuses the call, and a column naming nothing
/// takes a cell of any type.** A keyword field refuses an integer, as a build refuses an integer
/// column for it; an integer field refuses a fraction; a column that is no unique field is
/// ignored whatever it holds.
#[test]
fn an_address_cell_is_checked_against_its_field_alone() {
    use mosaica_engine::{AddressTable, AddressValue};
    let fx = fixture();
    let engine = engine_over(&fx);
    let table = |columns: Vec<(&str, AddressValue)>| AddressTable {
        rows: 1,
        tessera_id: None,
        columns: columns
            .into_iter()
            .map(|(name, cell)| (name.to_string(), vec![Some(cell)]))
            .collect(),
    };
    for (what, columns) in [
        ("an integer for a keyword", vec![("doi", AddressValue::Integer(10))]),
        (
            "a fraction for an integer",
            vec![("gid", AddressValue::Other("the number 1.5".to_string()))],
        ),
        (
            "a boolean for a keyword",
            vec![("doi", AddressValue::Other("true".to_string()))],
        ),
    ] {
        let refused = engine.name_items(&table(columns));
        assert!(
            matches!(refused, Err(mosaica_engine::EngineError::AddressMalformed(_))),
            "{what}: {refused:?}"
        );
    }
    let named = engine
        .name_items(&table(vec![
            ("doi", AddressValue::Text(doi_of(4))),
            ("score", AddressValue::Other("the number 0.5".to_string())),
            ("flags", AddressValue::Other("a list".to_string())),
            ("rank", AddressValue::Integer(3)),
        ]))
        .expect("columns naming nothing are ignored");
    assert_eq!(
        named.verdicts,
        vec![mosaica_lifecycle::resolve::Verdict::Names(built(&engine, 4))]
    );
    assert_eq!(named.ignored, ["score", "flags", "rank"]);
}

/// **A row giving an item a value another live or suppressed item holds names both, and is
/// refused**, whether the holder is built, buffered or flushed, and so is a batch setting one value
/// twice. A deleted holder names nothing, so its value may be given again. A row whose values name
/// only the holder edits it.
#[test]
fn an_ingest_setting_a_held_value_is_refused() {
    let fx = fixture();
    let engine = engine_over(&fx);
    let giving = |source: u64, doi: &str, gid: u64| {
        naming_built(
            &engine,
            row(&engine, "x", b"0", doi, gid, 1_000 + gid as i64),
            source,
        )
    };
    assert!(
        is_taken(try_ingest(
            &engine,
            "built",
            vec![giving(5, &doi_of(4), BIG * 3)]
        )),
        "a built holder"
    );

    let held = ingest(
        &engine,
        "held",
        vec![row(&engine, "h1", b"0", "10.h/1", BIG * 4, 2_000)],
    );
    assert!(
        is_taken(try_ingest(
            &engine,
            "buffered",
            vec![giving(5, "10.h/other", BIG * 4)]
        )),
        "a buffered holder of gid"
    );
    assert!(
        is_taken(try_ingest(
            &engine,
            "twice",
            vec![
                row(&engine, "t1", b"0", "10.t/1", BIG * 5, 3_000),
                row(&engine, "t2", b"0", "10.t/1", BIG * 5 + 1, 3_001),
            ]
        )),
        "one value twice in a batch"
    );
    publish_buffered(&engine);
    assert!(
        is_taken(try_ingest(
            &engine,
            "flushed",
            vec![giving(5, "10.h/1", BIG * 6)]
        )),
        "a flushed holder of doi"
    );

    // A suppressed holder still holds its value.
    engine.accept_change(held[0], ChangeOp::Suppress).unwrap();
    assert!(is_taken(try_ingest(
        &engine,
        "suppressed",
        vec![giving(5, "10.h/1", BIG * 7)]
    )));

    // A deleted one does not: the value goes to the item the row names.
    engine.accept_change(held[0], ChangeOp::Delete).unwrap();
    let edited = ingest(&engine, "again", vec![giving(5, "10.h/1", BIG * 4)]);
    let map = source_to_new_map(&fx.root, "v00000");
    assert_eq!(
        engine.tessera_id_of(edited[0]).unwrap(),
        engine.tessera_id_of(EntityId::new(map[&5])).unwrap(),
        "the row edits the item it names, which keeps its tessera_id"
    );
    assert_eq!(holders(&engine, "10.h/1"), 1);
    // A row naming only the holder edits it.
    let holder = ingest(
        &engine,
        "holder",
        vec![row(&engine, "fresh", b"0", "10.h/1", BIG * 4 + 1, 2_004)],
    );
    assert_eq!(
        engine.tessera_id_of(holder[0]).unwrap(),
        engine.tessera_id_of(edited[0]).unwrap()
    );
    assert_eq!(holders(&engine, "10.h/1"), 1);
    // Nulls never collide.
    let nulls = |id: &str| {
        let mut r = row(&engine, id, b"0", "unused", 0, 0);
        r.scalars = vec![WalScalar::Null, WalScalar::Null, WalScalar::Null, WalScalar::Null];
        r
    };
    ingest(&engine, "nulls", vec![nulls("z1"), nulls("z2")]);
}

/// **Batches racing to set one value leave one item holding it**, however the executor's queue
/// interleaves them: the first creates it, and each other one, checked again after it, names that
/// item and edits it or, where the item moved again while it was checked, is refused.
#[test]
fn two_concurrent_batches_setting_one_value_are_one_accepted() {
    let fx = fixture();
    let engine = Arc::new(engine_over(&fx));
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let engine = Arc::clone(&engine);
            std::thread::spawn(move || {
                let r = row(&engine, &format!("c{i}"), b"0", "10.race/1", BIG * 8 + i, 9_000 + i as i64);
                try_ingest(&engine, &format!("race-{i}"), vec![r])
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(outcomes.iter().any(|o| o.is_ok()), "{outcomes:?}");
    let named: BTreeSet<u64> = outcomes
        .iter()
        .filter_map(|o| o.as_ref().ok())
        .map(|entities| engine.tessera_id_of(entities[0]).unwrap().raw())
        .collect();
    assert_eq!(
        named.len(),
        1,
        "every accepted batch names one item: {outcomes:?}"
    );
    for outcome in outcomes.iter().filter(|o| o.is_err()) {
        assert!(taken(outcome), "{outcome:?}");
    }
    assert_eq!(holders(&engine, "10.race/1"), 1);
}

/// A row of the fixture's shape into `view`, joining `join` where the handler found one.
fn row_into(engine: &Engine, view: &str, id: &str, doi: &str, join: Option<EntityId>) -> UnallocatedRow {
    let mut r = row(engine, id, b"0", doi, 0, 0);
    r.scalars[1] = WalScalar::Null;
    r.scalars[2] = WalScalar::Null;
    r.scalars[3] = WalScalar::Utf8(format!("note-{doi}"));
    r.view = view.to_string();
    r.join = join;
    r
}

/// Submit `rows` on its own thread, holding it after its handler's check, and answer once it
/// holds there.
fn held_ingest(
    engine: &Arc<Engine>,
    batch: &'static str,
    rows: Vec<UnallocatedRow>,
) -> std::thread::JoinHandle<Result<Vec<EntityId>, AcceptError>> {
    engine.hold_next_write_check_for_test();
    let e = Arc::clone(engine);
    let handle = std::thread::spawn(move || try_ingest(&e, batch, rows));
    wait_until(
        "the ingest never reached its hold",
        Duration::from_secs(30),
        || engine.write_check_is_holding_for_test(),
    );
    handle
}

/// How many live items in views `s0` and `s1` hold `doi`, once every buffered row is flushed.
/// How many items hold `doi` in `s0`, once the buffer has flushed.
fn holders(engine: &Engine, doi: &str) -> usize {
    publish_buffered(engine);
    matching(engine, &full(engine), text_in("doi", &[doi.to_string()])).len()
}

fn holders_of(engine: &Engine, doi: &str) -> usize {
    publish_buffered(engine);
    let session = full(engine);
    let filter = || text_in("doi", &[doi.to_string()]);
    let mut found = matching_in(engine, &session, "s0", filter());
    found.extend(matching_in(engine, &session, "s1", filter()));
    found.len()
}

/// **A join that becomes a create on the executor is checked as a create.** The item a row joins
/// is deleted after the handler checked the row, so the row creates an item holding its value;
/// another batch setting that value in the same commit window is refused.
#[test]
fn a_join_that_becomes_a_create_is_checked_as_one() {
    let fx = fixture_of(true, &["s0", "s1"]);
    let engine = Arc::new(engine_over(&fx));
    let doi = "10.join/created";
    let joined = ingest(&engine, "first", vec![row_into(&engine, "s0", "j", doi, None)])[0];
    let held = held_ingest(
        &engine,
        "join",
        vec![row_into(&engine, "s1", "j", doi, Some(joined))],
    );
    engine.accept_change(joined, ChangeOp::Delete).unwrap();

    // The joining row and another batch setting its value reach the executor together.
    engine.set_work_pass_paused_for_test(true);
    let before = engine.work_enqueued_for_test();
    engine.release_write_check_for_test();
    let e = Arc::clone(&engine);
    let other = std::thread::spawn(move || {
        try_ingest(&e, "other", vec![row_into(&e, "s0", "k", doi, None)])
    });
    wait_until(
        "both batches never reached the queue",
        Duration::from_secs(30),
        || engine.work_enqueued_for_test() >= before + 2,
    );
    engine.set_work_pass_paused_for_test(false);
    let outcomes = [held.join().unwrap(), other.join().unwrap()];
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        1,
        "one batch holds the value: {outcomes:?}"
    );
    assert!(outcomes.iter().any(taken), "the other is refused: {outcomes:?}");
    assert_eq!(holders_of(&engine, doi), 1);
}

/// **A create that becomes a join on the executor carries its own value.** The handler found no
/// item holding the row's value, and another batch then created an item with that value; the row joins it and is accepted.
#[test]
fn a_create_that_becomes_a_join_carries_its_own_value() {
    let fx = fixture_of(true, &["s0", "s1"]);
    let engine = Arc::new(engine_over(&fx));
    let doi = "10.join/joined";
    let held = held_ingest(&engine, "late", vec![row_into(&engine, "s1", "x", doi, None)]);
    let created = ingest(&engine, "early", vec![row_into(&engine, "s0", "x", doi, None)])[0];
    engine.release_write_check_for_test();
    let joined = held.join().unwrap().expect("the row joins the item holding its value");
    assert_eq!(joined, vec![created], "the row joined the item its value names");
    assert_eq!(holders_of(&engine, doi), 1);
}

/// **A row joining an item carries the item's own value**, and is accepted.
#[test]
fn a_join_row_carrying_its_own_value_is_accepted() {
    let fx = fixture_of(true, &["s0", "s1"]);
    let engine = engine_over(&fx);
    let doi = "10.join/own";
    let item = ingest(&engine, "own", vec![row_into(&engine, "s0", "o", doi, None)])[0];
    let joined = ingest(&engine, "own-join", vec![row_into(&engine, "s1", "o", doi, Some(item))]);
    assert_eq!(joined, vec![item]);
    assert_eq!(holders_of(&engine, doi), 1);
}

/// **A join keeps its item's unique value when the view holding the item's own row is dropped
/// before that row flushes.** The drop discards the own row, so the join's row is the only record
/// of the value; after a flush and a restart past the log the value still finds the item.
#[test]
fn a_join_keeps_its_value_when_the_view_holding_its_own_row_is_dropped_before_it_flushes() {
    let fx = fixture();
    let engine = engine_over(&fx);
    engine
        .create_view_group(mosaica_lifecycle::wal::ViewGroupDeclaration {
            name: "quarter".to_string(),
            title: None,
            projection: "none".to_string(),
            frame: mosaica_engine::DeclaredFrame {
                x_min: 0.0,
                x_max: 1000.0,
                y_min: 0.0,
                y_max: 1000.0,
            },
            visibility: None,
            point_default: None,
            members: None,
            metadata: Vec::new(),
        })
        .expect("the group is declared");
    engine
        .create_view("quarter".into(), "q2".into(), None, Default::default())
        .expect("the view is created");

    let doi = "10.dropped/binding";
    let item = ingest(&engine, "own", vec![row_into(&engine, "quarter:q2", "d", doi, None)])[0];
    let joined = ingest(&engine, "join", vec![row_into(&engine, "s0", "d", doi, Some(item))]);
    assert_eq!(joined, vec![item], "the row joins the item");
    engine
        .drop_view("quarter".into(), "q2".into())
        .expect("the view drops");
    publish_buffered(&engine);

    let engine = restart(&fx, engine);
    assert!(
        engine.accepted_batch("join").is_none(),
        "the log rotated past the batches, so nothing but the stored value answers"
    );
    assert_eq!(
        unique_holders(&engine, "doi", &[doi.to_string()]).unwrap(),
        vec![Some(item)],
        "the value names its item"
    );
    assert_eq!(holders(&engine, doi), 1);
}

/// Declare one of the fixture's columns again, as the build declared it, with `unique` as given.
fn declare(engine: &Engine, name: &str, unique: bool) -> Result<bool, AcceptError> {
    let (ty, index, render) = match name {
        "doi" | "note" => ("keyword", false, false),
        "gid" => ("u64", true, false),
        "serial" => ("i64", false, true),
        other => panic!("the fixture declares no '{other}'"),
    };
    engine.declare_attribute(AttributeRequest {
        name: name.to_string(),
        title: None,
        ty: ty.to_string(),
        vocabulary: None,
        analyser: None,
        index,
        render,
        scope: LayerScope::Entity,
        unique,
    })
}

/// **`unique` declared at a running service answers what the build's declaration answers**, over
/// the same values, and refuses a duplicate from then on; the declaration survives a restart and
/// a fold.
#[test]
fn a_runtime_declaration_answers_as_the_build_declaration_does() {
    let built_fx = fixture_with(true);
    let built = engine_over(&built_fx);
    let expected_built = Expected::built(&built_fx, &built);

    let fx = fixture_with(false);
    let engine = engine_over(&fx);
    assert!(
        matching_result(&engine, text_in("doi", &[doi_of(1)])).is_err(),
        "before the declaration, a column with no index takes no filter"
    );
    // Rows buffered before the declaration are covered by it too.
    let early = ingest(
        &engine,
        "early",
        vec![row(&engine, "e1", b"0", "10.e/1", BIG * 9, 5_000)],
    );
    for name in ["doi", "gid", "serial"] {
        assert!(declare(&engine, name, true).is_ok(), "'{name}' holds no value twice");
    }
    assert!(
        is_taken(try_ingest(
            &engine,
            "early-dup",
            vec![naming_built(
                &engine,
                row(&engine, "e2", b"0", "10.e/1", BIG * 9 + 1, 5_001),
                2
            )]
        )),
        "a row buffered before the declaration holds its value"
    );
    publish_buffered(&engine);
    let mut expected = Expected::built(&fx, &engine);
    let early_id = engine.tessera_id_of(early[0]).unwrap().raw();
    expected.add(early_id, "10.e/1", BIG * 9, 5_000);
    check_lookups(&engine, &expected, "declared at runtime");
    // The same items answer on both bundles, named by the values they hold.
    let asked: Vec<String> = (0..N).step_by(7).map(doi_of).collect();
    let items = |engine: &Engine, expected: &Expected| -> BTreeSet<String> {
        let by_id: BTreeMap<u64, &String> =
            expected.by_doi.iter().map(|(doi, id)| (*id, doi)).collect();
        matching(engine, &full(engine), text_in("doi", &asked))
            .into_iter()
            .map(|id| by_id[&id].clone())
            .collect()
    };
    assert_eq!(items(&built, &expected_built), items(&engine, &expected));
    assert_eq!(items(&engine, &expected), asked.iter().cloned().collect());
    let verified = mosaica_build::verify_deep(&fx.root, &mosaica_build::VerifyOpts::default())
        .expect("the runtime-declared indexes agree with their columns");
    assert!(verified.unique_entries >= 3 * N, "every declared index was checked");

    assert!(is_taken(try_ingest(
        &engine,
        "dup",
        vec![naming_built(
            &engine,
            row(&engine, "d1", b"0", &doi_of(3), BIG * 10, 6_000),
            5
        )]
    )));
    let engine = restart(&fx, engine);
    check_lookups(&engine, &expected, "declared at runtime, after a restart");
    assert!(is_taken(try_ingest(
        &engine,
        "dup-2",
        vec![naming_built(
            &engine,
            row(&engine, "d2", b"0", &doi_of(3), BIG * 11, 6_001),
            5
        )]
    )));
    fold(&engine);
    let engine = restart(&fx, engine);
    check_lookups(&engine, &expected, "declared at runtime, after a fold and a restart");
    let verified = mosaica_build::verify_deep(&fx.root, &mosaica_build::VerifyOpts::default())
        .expect("the folded indexes agree with their columns");
    assert!(verified.unique_entries >= 3 * N, "every folded index was checked");
}

fn matching_result(engine: &Engine, filter: FilterExpr) -> Result<usize, String> {
    let session = full(engine);
    let mut req = ViewportRequest::new("s0", 0, VIEWPORT, 10_000);
    req.filter = Some(filter);
    engine
        .viewport(&session, req)
        .map(|out| out.points.len())
        .map_err(|e| e.to_string())
}

/// **A column already holding a value twice is refused, and stays as it was**; so is one whose
/// duplicate is only in the buffer.
#[test]
fn a_runtime_declaration_over_duplicates_is_refused() {
    let fx = fixture_with(false);
    let engine = engine_over(&fx);
    assert!(
        matches!(
            declare(&engine, "note", true),
            Err(AcceptError::Exec(ExecError::UniqueTaken { .. }))
        ),
        "a column holding values twice is refused"
    );
    assert!(!engine.meta().declared_scalars.iter().any(|d| d.unique && d.name != "id"));

    // A duplicate held only by a buffered row.
    let twin = ingest(
        &engine,
        "twin",
        vec![row(&engine, "tw", b"0", &doi_of(2), BIG * 12, 7_000)],
    )[0];
    assert!(matches!(
        declare(&engine, "doi", true),
        Err(AcceptError::Exec(ExecError::UniqueTaken { .. }))
    ));
    // And after the duplicate is flushed.
    publish_buffered(&engine);
    assert!(matches!(
        declare(&engine, "doi", true),
        Err(AcceptError::Exec(ExecError::UniqueTaken { .. }))
    ));
    // The values are distinct once the twin is deleted, and the declaration then holds.
    engine.accept_change(twin, ChangeOp::Delete).unwrap();
    assert!(declare(&engine, "doi", true).is_ok());
}

/// Declare `name` unique on its own thread, holding its build after the first round, and answer
/// once the round holds.
fn held_declaration(
    engine: &Arc<Engine>,
    name: &'static str,
) -> std::thread::JoinHandle<Result<bool, AcceptError>> {
    engine.set_unique_round_paused_for_test(true);
    let e = Arc::clone(engine);
    let handle = std::thread::spawn(move || declare(&e, name, true));
    wait_until(
        "the declaration's round never held",
        Duration::from_secs(60),
        || engine.unique_round_is_holding_for_test(),
    );
    handle
}

fn is_unique(engine: &Engine, name: &str) -> bool {
    engine.meta().declared_scalars.iter().any(|d| d.name == name && d.unique)
}

/// **A duplicate arriving while a declaration builds refuses it**, whether it is still buffered
/// when the build ends or a flush published it during a round.
#[test]
fn a_duplicate_arriving_mid_build_refuses_the_declaration() {
    for flushed in [false, true] {
        let fx = fixture_with(false);
        let engine = Arc::new(engine_over(&fx));
        let held = held_declaration(&engine, "doi");
        ingest(&engine, "twin", vec![row(&engine, "tw", b"0", &doi_of(4), BIG + 13, 7_100)]);
        if flushed {
            publish_buffered(&engine);
        }
        engine.set_unique_round_paused_for_test(false);
        assert!(
            matches!(held.join().unwrap(), Err(AcceptError::Exec(ExecError::UniqueTaken { .. }))),
            "flushed: {flushed}"
        );
        assert!(!is_unique(&engine, "doi"));
    }
}

/// **A value arriving while a declaration builds is held by it**, buffered or flushed during a
/// round: once the declaration answers, the value refuses a second holder and is found.
#[test]
fn a_value_arriving_mid_build_is_indexed() {
    for flushed in [false, true] {
        let fx = fixture_with(false);
        let engine = Arc::new(engine_over(&fx));
        let doi = format!("10.mid/{flushed}");
        let held = held_declaration(&engine, "doi");
        let item = ingest(&engine, "mid", vec![row(&engine, "mid", b"0", &doi, BIG + 14, 7_200)])[0];
        if flushed {
            publish_buffered(&engine);
        }
        engine.set_unique_round_paused_for_test(false);
        held.join().unwrap().expect("no value is held twice");
        assert!(is_taken(try_ingest(
            &engine,
            "second",
            vec![naming_built(
                &engine,
                row(&engine, "second", b"0", &doi, BIG + 15, 7_201),
                3
            )]
        )));
        if !flushed {
            publish_buffered(&engine);
        }
        let id = engine.tessera_id_of(item).unwrap().raw();
        assert_eq!(
            matching(
                &engine,
                &full(&engine),
                text_in("doi", std::slice::from_ref(&doi))
            ),
            BTreeSet::from([id])
        );
    }
}

/// **An edit of a value's holder while `unique` is declared leaves the value held by the item.**
/// The edit moves the holder to a new entity: acknowledged or flushed while the declaration builds,
/// or checked before it and admitted after. The index names the item's new entity, so a second
/// item cannot take the value and the value finds the item, across a restart.
#[test]
fn an_edit_of_a_holder_during_a_declaration_keeps_its_value_held() {
    for when in ["buffered", "flushed", "admitted after"] {
        let fx = fixture_with(false);
        let mut engine = Arc::new(engine_over(&fx));
        let doi = doi_of(4);
        let holder = EntityId::new(source_to_new_map(&fx.root, "v00000")[&4]);
        let tid = engine.tessera_id_of(holder).unwrap();
        let edit = move |engine: &Engine| {
            fill(
                engine,
                "edit",
                "note",
                vec![(holder, WalScalar::Utf8(format!("edited {when}")))],
            )
        };
        if when == "admitted after" {
            engine.hold_next_write_check_for_test();
            let e = Arc::clone(&engine);
            let held = std::thread::spawn(move || edit(&e));
            wait_until(
                "the edit never reached its hold",
                Duration::from_secs(30),
                || engine.write_check_is_holding_for_test(),
            );
            assert!(declare(&engine, "doi", true).expect("no two items hold one value"));
            engine.release_write_check_for_test();
            held.join().unwrap().expect("the edit is taken");
        } else {
            let held = held_declaration(&engine, "doi");
            edit(&engine).expect("the edit is taken");
            if when == "flushed" {
                publish_buffered(&engine);
            }
            engine.set_unique_round_paused_for_test(false);
            held.join().unwrap().expect("no two items hold one value");
        }
        assert!(is_unique(&engine, "doi"), "{when}");
        assert_ne!(
            engine.resolve_tessera_ids(&[tid]).unwrap()[0],
            Some(holder),
            "the edit moved the holder, {when}"
        );
        for pass in ["the declaration", "a restart"] {
            assert!(
                is_taken(try_ingest(
                    &engine,
                    "second",
                    vec![naming_built(
                        &engine,
                        row(&engine, "second", b"0", &doi, BIG + 16, 7_300),
                        3
                    )]
                )),
                "a second item cannot take {doi}, {when}, after {pass}"
            );
            publish_buffered(&engine);
            assert_eq!(
                matching(
                    &engine,
                    &full(&engine),
                    text_in("doi", std::slice::from_ref(&doi))
                ),
                BTreeSet::from([tid.raw()]),
                "{doi} finds the edited item, {when}, after {pass}"
            );
            let owned = Arc::try_unwrap(engine)
                .ok()
                .expect("no other handle is held");
            engine = Arc::new(restart(&fx, owned));
        }
    }
}

/// **A declaration and a fold do not overlap, and each waits for the other**: a declaration made
/// while a fold runs is built after it, and a fold requested while a declaration builds runs
/// after it. Both end with the index answering, across a restart, and agreeing with its column.
#[test]
fn a_declaration_and_a_fold_wait_for_each_other() {
    let fx = fixture_with(false);
    let engine = Arc::new(engine_over(&fx));
    let expected = Expected::built(&fx, &engine);

    // A fold in flight, then a declaration.
    engine.set_fold_paused_for_test(true);
    let folds = engine.write_executor_stats().folds;
    engine.request_fold();
    wait_until("the fold never held", Duration::from_secs(60), || {
        engine.fold_is_holding_for_test()
    });
    let e = Arc::clone(&engine);
    let during_fold = std::thread::spawn(move || declare(&e, "doi", true));
    engine.set_fold_paused_for_test(false);
    during_fold.join().unwrap().expect("the declaration is built after the fold");
    wait_until("the fold never published", Duration::from_secs(60), || {
        engine.write_executor_stats().folds > folds
    });

    // A declaration building, then a fold.
    let held = held_declaration(&engine, "gid");
    let folds = engine.write_executor_stats().folds;
    engine.request_fold();
    engine.set_unique_round_paused_for_test(false);
    held.join().unwrap().expect("the declaration is built before the fold");
    wait_until("the fold never published", Duration::from_secs(60), || {
        engine.write_executor_stats().folds > folds
    });
    assert!(declare(&engine, "serial", true).is_ok());

    let engine = restart(&fx, Arc::try_unwrap(engine).ok().expect("one holder"));
    check_lookups(&engine, &expected, "declared across two folds, after a restart");
    let verified = mosaica_build::verify_deep(&fx.root, &mosaica_build::VerifyOpts::default())
        .expect("the indexes agree with their columns");
    assert!(verified.unique_entries >= 3 * N);
}

/// **Removing `unique` keeps the values and stops refusing**, and survives a restart.
#[test]
fn removing_unique_keeps_the_values() {
    let fx = fixture();
    let engine = engine_over(&fx);
    assert!(declare(&engine, "gid", false).is_ok());
    assert!(!engine
        .meta()
        .declared_scalars
        .iter()
        .find(|d| d.name == "gid")
        .unwrap()
        .unique);
    // The column's own index still answers, since it was declared `index`.
    let session = full(&engine);
    assert_eq!(matching(&engine, &session, number_in("gid", &[gid_of(5) as i128])).len(), 1);
    ingest(
        &engine,
        "same-gid",
        vec![row(&engine, "g1", b"0", "10.g/1", gid_of(5), 8_000)],
    );
    publish_buffered(&engine);
    let session = full(&engine);
    assert_eq!(matching(&engine, &session, number_in("gid", &[gid_of(5) as i128])).len(), 2);
    let engine = restart(&fx, engine);
    assert!(!engine
        .meta()
        .declared_scalars
        .iter()
        .find(|d| d.name == "gid")
        .unwrap()
        .unique);
    let session = full(&engine);
    assert_eq!(matching(&engine, &session, number_in("gid", &[gid_of(5) as i128])).len(), 2);
}

/// Set one column's value on items that exist: an ingest batch naming no view, each row naming
/// its item by `tessera_id` and carrying that column alone.
fn fill(
    engine: &Engine,
    batch: &str,
    column: &str,
    rows: Vec<(EntityId, WalScalar)>,
) -> Result<(), AcceptError> {
    let declared = engine.meta().declared_scalars;
    let at = declared
        .iter()
        .position(|d| d.name == column)
        .expect("a declared column");
    let rows = rows
        .into_iter()
        .map(|(entity, value)| {
            let mut scalars = vec![WalScalar::Null; declared.len()];
            scalars[at] = value;
            mosaica_engine::IngestRow {
                tessera_id: Some(engine.tessera_id_of(entity).unwrap()),
                labels: None,
                position: None,
                scalars,
                scoped: Vec::new(),
                omitted: (0..declared.len()).filter(|p| *p != at).collect(),
            }
        })
        .collect();
    engine
        .ingest(mosaica_engine::IngestRequest {
            batch_id: batch.to_string(),
            body_hash: hash_of(batch),
            view: None,
            rows,
            artifacts: Default::default(),
            strict: true,
            tessera_id_column: false,
        })
        .map(|_| ())
}

/// **A row without coordinates giving an item a unique value another item holds is refused**, as
/// one with them is, and a free value is held from its acknowledgement and indexed by its flush.
#[test]
fn a_values_fill_setting_a_held_value_is_refused() {
    let fx = fixture();
    let engine = engine_over(&fx);
    let mut blank = row(&engine, "blank", b"0", "unused", 0, 0);
    blank.scalars = vec![WalScalar::Null, WalScalar::Null, WalScalar::Null, WalScalar::Null];
    let other = {
        let mut r = row(&engine, "other", b"0", "unused", 0, 0);
        r.scalars = vec![WalScalar::Null, WalScalar::Null, WalScalar::Null, WalScalar::Null];
        r
    };
    let entities = ingest(&engine, "blanks", vec![blank, other]);
    publish_buffered(&engine);
    let taken = |r: Result<(), AcceptError>| matches!(r, Err(AcceptError::Conflict(_)));
    assert!(taken(fill(
        &engine,
        "f1",
        "doi",
        vec![(entities[0], WalScalar::Utf8(doi_of(3)))]
    )));
    assert!(taken(fill(
        &engine,
        "f2",
        "doi",
        vec![
            (entities[0], WalScalar::Utf8("10.f/1".to_string())),
            (entities[1], WalScalar::Utf8("10.f/1".to_string())),
        ]
    )));
    fill(&engine, "f3", "doi", vec![(entities[0], WalScalar::Utf8("10.f/1".to_string()))])
        .expect("a free value fills");
    assert!(
        taken(fill(&engine, "f4", "doi", vec![(entities[1], WalScalar::Utf8("10.f/1".to_string()))])),
        "a buffered fill holds its value"
    );
    publish_buffered(&engine);
    let session = full(&engine);
    let id = engine.tessera_id_of(entities[0]).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, text_in("doi", &["10.f/1".to_string()])),
        BTreeSet::from([id])
    );
}

/// **A declaration landing between a batch's check and its admission is seen.** The batch was
/// checked while the column was not unique; admitted after the declaration, it is checked again
/// under it, and no second item holds the value.
#[test]
fn a_declaration_between_a_batchs_check_and_its_admission_is_checked_again() {
    let fx = fixture_with(false);
    let engine = Arc::new(engine_over(&fx));
    let doi = doi_of(4);
    let held = held_ingest(
        &engine,
        "late",
        vec![row(&engine, "late", b"0", &doi, BIG + 40, 40_000)],
    );
    assert!(declare(&engine, "doi", true).expect("no two items hold one value yet"));
    engine.release_write_check_for_test();
    // Under the declaration the row names the item holding the value, and edits it.
    let outcome = held
        .join()
        .unwrap()
        .expect("the row edits the item holding the value");
    let map = source_to_new_map(&fx.root, "v00000");
    assert_eq!(
        engine.tessera_id_of(outcome[0]).unwrap(),
        engine.tessera_id_of(EntityId::new(map[&4])).unwrap(),
        "the row names the item holding {doi}"
    );
    assert_eq!(holders(&engine, &doi), 1, "one item holds {doi}");
}

/// **A row setting a value checked before an ingest took it is checked again, and refused.**
#[test]
fn a_fill_whose_value_is_taken_after_its_check_is_refused() {
    let fx = fixture();
    let engine = Arc::new(engine_over(&fx));
    let mut blank = row(&engine, "blank", b"0", "unused", 0, 0);
    blank.scalars = vec![WalScalar::Null, WalScalar::Null, WalScalar::Null, WalScalar::Null];
    let entity = ingest(&engine, "blank", vec![blank])[0];
    publish_buffered(&engine);
    let doi = "10.fill/raced";
    engine.hold_next_write_check_for_test();
    let e = Arc::clone(&engine);
    let held = std::thread::spawn(move || {
        fill(&e, "raced", "doi", vec![(entity, WalScalar::Utf8(doi.to_string()))])
    });
    wait_until(
        "the fill never reached its hold",
        Duration::from_secs(30),
        || engine.write_check_is_holding_for_test(),
    );
    ingest(
        &engine,
        "taker",
        vec![row(&engine, "taker", b"0", doi, BIG + 30, 30_000)],
    );
    engine.release_write_check_for_test();
    // Checked again after the ingest, the row names its item and the value's new holder.
    assert!(matches!(
        held.join().unwrap(),
        Err(AcceptError::Conflict(_))
    ));
    publish_buffered(&engine);
    assert_eq!(matching(&engine, &full(&engine), text_in("doi", &[doi.to_string()])).len(), 1);
}

/// **A flush planned before a declaration is planned again after it**, so the rows it carried
/// reach the new index through a run rather than being lost between the build and the flush.
#[test]
fn a_flush_in_flight_across_a_declaration_is_planned_again() {
    let fx = fixture_with(false);
    let engine = engine_over(&fx);
    let entities = ingest(
        &engine,
        "in-flight",
        vec![row(&engine, "f1", b"0", "10.fl/1", BIG * 14, 11_000)],
    );
    engine.set_flush_paused_for_test(true);
    engine.request_flush();
    wait_until(
        "the flush to hold",
        std::time::Duration::from_secs(30),
        || engine.flush_is_holding_for_test(),
    );
    assert!(declare(&engine, "doi", true).is_ok());
    engine.set_flush_paused_for_test(false);
    publish_buffered(&engine);
    let session = full(&engine);
    let id = engine.tessera_id_of(entities[0]).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, text_in("doi", &["10.fl/1".to_string()])),
        BTreeSet::from([id])
    );
    let engine = restart(&fx, engine);
    assert_eq!(
        matching(&engine, &full(&engine), text_in("doi", &["10.fl/1".to_string()])),
        BTreeSet::from([id]),
        "the row reached a run, not only the live entries a restart rebuilds from the buffer"
    );
}

/// **A unique column declared new at a running service** holds no value, refuses a duplicate
/// from its first rows, and answers lookups through a flush and a restart.
#[test]
fn a_new_unique_column_declared_at_runtime_is_enforced_and_indexed() {
    let fx = fixture();
    let engine = engine_over(&fx);
    assert!(!engine
        .declare_attribute(AttributeRequest {
            name: "isbn".to_string(),
            title: None,
            ty: "keyword".to_string(),
            vocabulary: None,
            analyser: None,
            index: false,
            render: false,
            scope: LayerScope::Entity,
            unique: true,
        })
        .expect("a new unique column is declared"));
    let with_isbn = |engine: &Engine, id: &str, doi: &str, gid: u64, isbn: &str| {
        let mut r = row(engine, id, b"0", doi, gid, gid as i64 % 1_000_000 + 50_000);
        // `id`, which no row here holds, then the new column.
        r.scalars.extend([WalScalar::Null, WalScalar::Utf8(isbn.to_string())]);
        r
    };
    let first = ingest(&engine, "isbn-1", vec![with_isbn(&engine, "i1", "10.i/1", BIG * 15, "978-1")]);
    assert!(is_taken(try_ingest(
        &engine,
        "isbn-2",
        vec![naming_built(
            &engine,
            with_isbn(&engine, "i2", "10.i/2", BIG * 15 + 1, "978-1"),
            3
        )]
    )));
    publish_buffered(&engine);
    let id = engine.tessera_id_of(first[0]).unwrap().raw();
    assert_eq!(
        matching(&engine, &full(&engine), text_in("isbn", &["978-1".to_string()])),
        BTreeSet::from([id])
    );
    let engine = restart(&fx, engine);
    assert_eq!(
        matching(&engine, &full(&engine), text_in("isbn", &["978-1".to_string()])),
        BTreeSet::from([id])
    );
    assert!(is_taken(try_ingest(
        &engine,
        "isbn-3",
        vec![naming_built(
            &engine,
            with_isbn(&engine, "i3", "10.i/3", BIG * 15 + 2, "978-1"),
            3
        )]
    )));
}
