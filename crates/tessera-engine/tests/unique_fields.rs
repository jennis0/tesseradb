//! **Unique fields at a running service**: `eq` and `in` on a unique column answer from its index
//! and equal the items that hold the values, before and after a flush, a coalesce, a fold and a
//! restart; a holder the viewer cannot see answers as absent; an ingest or a values fill giving
//! an item a value another item holds is refused; `unique` declared on a column that exists is
//! built over its values and refused where two items hold one value; removing it keeps the
//! values.
//!
//! The fixture declares three unique columns at the build: `doi`, a keyword with no other home
//! than the record blob, so `eq` and `in` are the only filters it takes; `gid`, an indexed `u64`
//! whose values pass 2^53; and `serial`, a rendered `i64` holding negative values.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, Int64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_engine::filter::{FilterExpr, FilterOperand, Scalar};
use tessera_engine::{AcceptError, AttributeRequest, Engine, EngineConfig, Session, ViewportRequest};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::{ChangeOp, ExecError};
use tessera_types::layer::LayerScope;
use tessera_types::EntityId;

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
                ids.iter().map(|e| ((e * 53) % 1000) as f64).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(ids.iter().map(|e| doi_of(*e)).collect::<Vec<_>>())),
            Arc::new(UInt64Array::from(ids.iter().map(|e| gid_of(*e)).collect::<Vec<_>>())),
            Arc::new(Int64Array::from(ids.iter().map(|e| serial_of(*e)).collect::<Vec<_>>())),
            Arc::new(StringArray::from(ids.iter().map(|e| note_of(*e)).collect::<Vec<_>>())),
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
    let schema = tessera_build::config::Config::parse(&schema_path, &Default::default())
        .expect("the fixture schema parses")
        .schema;
    tessera_build::build(&tessera_build::BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
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
        attribute_sources: tessera_build::config::AttributeSource::over(points, &schema),
        out: root.clone(),
        limit: None,
        identity_key: test_key(),
        identity_key_hex: TEST_KEY_HEX.to_string(),
        idset: 1,
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        mint_external_ids: true,
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
        tessera_plugin::Passthrough::new(),
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
        join_only: false,
        external_id: Some(id.as_bytes().to_vec()),
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
    engine.accept_ingest(rows, batch.to_string(), hash_of(batch))
}

fn ingest(engine: &Engine, batch: &str, rows: Vec<UnallocatedRow>) -> Vec<EntityId> {
    try_ingest(engine, batch, rows).unwrap_or_else(|e| panic!("batch {batch} is accepted: {e}"))
}

fn is_taken(result: Result<Vec<EntityId>, AcceptError>) -> bool {
    taken(&result)
}

fn taken(result: &Result<Vec<EntityId>, AcceptError>) -> bool {
    matches!(result, Err(AcceptError::Exec(ExecError::UniqueTaken { .. })))
}

fn full(engine: &Engine) -> Session {
    engine.authorise(&full_coverage_credential()).unwrap()
}

/// The `tessera_id`s a viewport under `filter` returns.
fn matching(engine: &Engine, session: &Session, filter: FilterExpr) -> BTreeSet<u64> {
    let mut req = ViewportRequest::new("s0", 0, VIEWPORT, 10_000);
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
    flush(&engine);
    check_lookups(&engine, &expected, "after one flush");
    add(&engine, "b", 4, &mut expected);
    flush(&engine);
    add(&engine, "c", 2, &mut expected);
    flush(&engine);

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
    let buffered = add(&engine, "d", 2, &mut expected);
    let engine = restart(&fx, engine);
    // The rows the log held are live entries again: their values are held.
    let doi = format!("10.d/{}", 0);
    assert!(is_taken(try_ingest(
        &engine,
        "after-restart",
        vec![row(&engine, "r", b"0", &doi, BIG * 13, 7_777)]
    )));
    flush(&engine);
    let _ = buffered;
    check_lookups(&engine, &expected, "after the restart's flush");
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

/// **An ingest giving an item a value another live or suppressed item holds is refused**, whether
/// the holder is built, buffered or flushed, and so is a batch setting one value twice. A deleted
/// holder names nothing, so its value may be given again.
#[test]
fn an_ingest_setting_a_held_value_is_refused() {
    let fx = fixture();
    let engine = engine_over(&fx);
    let built = |s| row(&engine, &format!("x{s}"), b"0", &doi_of(s), BIG * 3 + s, 1_000 + s as i64);
    assert!(is_taken(try_ingest(&engine, "built", vec![built(4)])), "a built holder");

    let held = ingest(
        &engine,
        "held",
        vec![row(&engine, "h1", b"0", "10.h/1", BIG * 4, 2_000)],
    );
    assert!(
        is_taken(try_ingest(
            &engine,
            "buffered",
            vec![row(&engine, "h2", b"0", "10.h/other", BIG * 4, 2_001)]
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
    flush(&engine);
    assert!(
        is_taken(try_ingest(
            &engine,
            "flushed",
            vec![row(&engine, "h3", b"0", "10.h/1", BIG * 6, 2_002)]
        )),
        "a flushed holder of doi"
    );

    // A suppressed holder still holds its value.
    engine.accept_change(held[0], ChangeOp::Suppress).unwrap();
    assert!(is_taken(try_ingest(
        &engine,
        "suppressed",
        vec![row(&engine, "h4", b"0", "10.h/1", BIG * 7, 2_003)]
    )));

    // A deleted one does not.
    engine.accept_change(held[0], ChangeOp::Delete).unwrap();
    ingest(
        &engine,
        "again",
        vec![row(&engine, "h5", b"0", "10.h/1", BIG * 4, 2_000)],
    );
    // Nulls never collide.
    let nulls = |id: &str| {
        let mut r = row(&engine, id, b"0", "unused", 0, 0);
        r.scalars = vec![WalScalar::Null, WalScalar::Null, WalScalar::Null, WalScalar::Null];
        r
    };
    ingest(&engine, "nulls", vec![nulls("z1"), nulls("z2")]);
}

/// **Two batches setting one value, admitted into one commit window, are one accepted and one
/// refused**, however the executor's queue interleaves them.
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
    let accepted = outcomes.iter().filter(|o| o.is_ok()).count();
    assert_eq!(accepted, 1, "exactly one batch holds the value: {outcomes:?}");
    assert!(outcomes.iter().filter(|o| o.is_err()).all(|o| taken(o)
        || matches!(o, Err(AcceptError::Exec(ExecError::UniqueStale)))));
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
        matches!(
            matching_result(&engine, text_in("doi", &[doi_of(1)])),
            Err(_)
        ),
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
            vec![row(&engine, "e2", b"0", "10.e/1", BIG * 9 + 1, 5_001)]
        )),
        "a row buffered before the declaration holds its value"
    );
    flush(&engine);
    let mut expected = Expected::built(&fx, &engine);
    let early_id = engine.tessera_id_of(early[0]).unwrap().raw();
    expected.add(early_id, "10.e/1", BIG * 9, 5_000);
    check_lookups(&engine, &expected, "declared at runtime");
    // The same items answer on both bundles, by source.
    let session = full(&built);
    let asked: Vec<String> = (0..N).step_by(7).map(doi_of).collect();
    let on_built = matching(&built, &session, text_in("doi", &asked)).len();
    let on_runtime = matching(&engine, &full(&engine), text_in("doi", &asked)).len();
    assert_eq!(on_built, on_runtime);
    assert_eq!(on_built, asked.len());
    let _ = expected_built;

    assert!(is_taken(try_ingest(
        &engine,
        "dup",
        vec![row(&engine, "d1", b"0", &doi_of(3), BIG * 10, 6_000)]
    )));
    let engine = restart(&fx, engine);
    check_lookups(&engine, &expected, "declared at runtime, after a restart");
    assert!(is_taken(try_ingest(
        &engine,
        "dup-2",
        vec![row(&engine, "d2", b"0", &doi_of(3), BIG * 11, 6_001)]
    )));
    fold(&engine);
    let engine = restart(&fx, engine);
    check_lookups(&engine, &expected, "declared at runtime, after a fold and a restart");
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
/// duplicate is only in the buffer. The refusal names no entity.
#[test]
fn a_runtime_declaration_over_duplicates_is_refused() {
    let fx = fixture_with(false);
    let engine = engine_over(&fx);
    match declare(&engine, "note", true) {
        Err(AcceptError::Exec(ExecError::UniqueTaken { detail })) => {
            assert!(detail.contains("'n1'"), "names a value held twice: {detail}");
        }
        other => panic!("a column holding values twice is refused: {other:?}"),
    }
    assert!(!engine.meta().declared_scalars.iter().any(|d| d.unique));

    // A duplicate held only by a buffered row.
    ingest(
        &engine,
        "twin",
        vec![row(&engine, "tw", b"0", &doi_of(2), BIG * 12, 7_000)],
    );
    assert!(matches!(
        declare(&engine, "doi", true),
        Err(AcceptError::Exec(ExecError::UniqueTaken { .. }))
    ));
    // And after the duplicate is flushed.
    flush(&engine);
    assert!(matches!(
        declare(&engine, "doi", true),
        Err(AcceptError::Exec(ExecError::UniqueTaken { .. }))
    ));
    // The values are distinct once the twin is deleted, and the declaration then holds.
    let twin = engine
        .resolve_external_id(b"tw")
        .unwrap()
        .expect("the twin is held");
    engine.accept_change(twin, ChangeOp::Delete).unwrap();
    assert!(declare(&engine, "doi", true).is_ok());
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
    flush(&engine);
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

fn fill(engine: &Engine, batch: &str, column: &str, rows: Vec<(EntityId, WalScalar)>) -> Result<(), AcceptError> {
    engine
        .fill_values(tessera_lifecycle::ValuesRequest {
            batch_id: batch.to_string(),
            body_hash: hash_of(batch),
            view: Some("s0".to_string()),
            columns: vec![column.to_string()],
            rows: rows
                .into_iter()
                .map(|(entity, value)| tessera_lifecycle::IncomingValues {
                    entity,
                    values: vec![value],
                })
                .collect(),
            artifacts: Default::default(),
        })
        .map(|_| ())
}

/// **A values fill giving an item a unique value another item holds is refused**, as an ingest
/// is, and a fill of a free value is held from its acknowledgement and indexed by its flush.
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
    flush(&engine);
    let taken = |r: Result<(), AcceptError>| {
        matches!(r, Err(AcceptError::Exec(ExecError::UniqueTaken { .. })))
    };
    assert!(taken(fill(&engine, "f1", "doi", vec![(entities[0], WalScalar::Utf8(doi_of(3)))])));
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
    flush(&engine);
    let session = full(&engine);
    let id = engine.tessera_id_of(entities[0]).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, text_in("doi", &["10.f/1".to_string()])),
        BTreeSet::from([id])
    );
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
    wait_until("the flush to hold", std::time::Duration::from_secs(30), || {
        engine.flush_is_holding_for_test()
    });
    assert!(declare(&engine, "doi", true).is_ok());
    engine.set_flush_paused_for_test(false);
    flush(&engine);
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
