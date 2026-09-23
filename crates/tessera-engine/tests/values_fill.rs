//! **Attribute values filled on entities that already exist** (`POST /control/values`,
//! `ingest.md` §1.4; decision 0136, track T3): every family fills on an entity that predates the
//! value and reads back, the fill rule accepts a restatement and refuses a change, a fill on an
//! entity whose blob row has already flushed is read through the record stack's per-column
//! claimant read, a tick whose only work is fills publishes them with no new segment, and a
//! restart replays the batch and writes its cells once.
//!
//! The fixture is `runtime_attributes.rs`': two columns declared at the build, so a runtime
//! column appends after positions the build already filled.

mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float32Array, Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_engine::filter::{Endpoint, FilterExpr, FilterOperand, Scalar};
use tessera_engine::{
    AcceptError, AttributeRequest, Engine, IncomingValues, ScalarOut, Session, ValuesRequest,
    ViewportRequest,
};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::ExecError;
use tessera_types::layer::LayerScope;
use tessera_types::EntityId;

const N_ITEMS: u64 = 60;
const VIEWPORT: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "band"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  low = 1
  mid = 2
  high = 3

[[vocabulary]]
name       = "dept"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  eng = 5
  ops = 6
  legal = 7

[[attribute]]
name       = "band"
type       = "category"
render     = true
vocabulary = "band"

[[attribute]]
name   = "score"
type   = "f32"
render = true
index  = true
"#;

fn band_of(entity: u64) -> &'static str {
    match entity % 3 {
        0 => "low",
        1 => "mid",
        _ => "high",
    }
}

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("band", DataType::Utf8, false),
        Field::new("score", DataType::Float32, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let bands: Vec<&str> = ids.iter().map(|e| band_of(*e)).collect();
    let scores: Vec<f32> = ids.iter().map(|e| (e % 97) as f32 * 0.5).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(bands)),
            Arc::new(Float32Array::from(scores)),
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

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);
    let schema_path = tmp.path().join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
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
    .expect("a build with a declared schema succeeds");
    Fixture { tmp, root }
}

fn engine_over(fx: &Fixture) -> Engine {
    let mut engine = Engine::open(
        &fx.root,
        &fx.tmp.path().join("cache"),
        &fx.tmp.path().join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config_uncapped(),
    )
    .expect("the engine opens");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

fn restart(fx: &Fixture, engine: Engine) -> Engine {
    drop(engine);
    engine_over(fx)
}

fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == before {
        let now = engine.write_executor_stats();
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published: {} failures, {} flushable items, {} skips",
            now.flush_failures,
            now.flushable_items,
            now.flush_skips,
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Ask for a flush and wait, without insisting one is owed — what a restart needs, since the
/// period tick may already have published the fills this test is about.
fn settle(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.write_executor_stats().flushes == before && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn declare(engine: &Engine, name: &str, ty: &str, index: bool) {
    let vocabulary = (ty == "category").then(|| "dept".to_string());
    engine
        .declare_attribute(AttributeRequest {
            name: name.to_string(),
            title: None,
            ty: ty.to_string(),
            vocabulary,
            analyser: None,
            index,
            render: false,
            scope: LayerScope::Entity,
        })
        .unwrap_or_else(|e| panic!("column '{name}' declares: {e}"));
}

/// The five runtime families this file fills: one per home the schema can put a value in.
///
/// `note` is neither indexed nor rendered, so it is blob-resident — the column whose fill on a
/// flushed entity needs the record stack's per-column claimant read. `tag`, `dept` and `weight`
/// take entity-space value columns, and `prose` a text layer. A rendered column is not among them
/// because `PUT /control/attributes` refuses `render` (decision 0136's amendment); the fixture's
/// build columns `band` and `score` are the rendered ones the values route meets.
fn declare_families(engine: &Engine) {
    declare(engine, "note", "keyword", false);
    declare(engine, "tag", "keyword", true);
    declare(engine, "prose", "text", true);
    declare(engine, "dept", "category", true);
    declare(engine, "weight", "f32", true);
}

const FAMILIES: [&str; 5] = ["note", "tag", "prose", "dept", "weight"];

fn filled_values() -> Vec<WalScalar> {
    vec![
        WalScalar::Utf8("a private note".to_string()),
        WalScalar::Utf8("alpha".to_string()),
        WalScalar::Utf8("the quick brown fox".to_string()),
        // **A category travels as its code below the door.** `/control/values` resolves a key to
        // one exactly as `/control/ingest` does (`category_code`), and this test submits to the
        // engine, which is the other side of that resolution.
        WalScalar::U8(6),
        WalScalar::F32(12.5),
    ]
}

fn row(external_id: &str, terms: &Engine, scalars: Vec<WalScalar>) -> UnallocatedRow {
    UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 500.0,
        y: 500.0,
        scalars,
        terms: terms.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    }
}

fn build_columns(band: &str, score: f32) -> Vec<WalScalar> {
    vec![WalScalar::Utf8(band.to_string()), WalScalar::F32(score)]
}

fn ingest(engine: &Engine, batch: &str, rows: Vec<UnallocatedRow>) -> Vec<EntityId> {
    let mut hash = [0u8; 32];
    hash[..batch.len().min(32)].copy_from_slice(&batch.as_bytes()[..batch.len().min(32)]);
    engine
        .accept_ingest(rows, batch.to_string(), hash)
        .unwrap_or_else(|e| panic!("batch {batch} is accepted: {e}"))
}

fn values_request(
    batch: &str,
    columns: &[&str],
    rows: Vec<(EntityId, Vec<WalScalar>)>,
) -> ValuesRequest {
    let mut hash = [0u8; 32];
    hash[..batch.len().min(32)].copy_from_slice(&batch.as_bytes()[..batch.len().min(32)]);
    ValuesRequest {
        batch_id: batch.to_string(),
        body_hash: hash,
        view: Some("s0".to_string()),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        rows: rows
            .into_iter()
            .map(|(entity, values)| IncomingValues { entity, values })
            .collect(),
        artifacts: Default::default(),
    }
}

/// How many segments the bundle's partitions name, for the values-only tick's claim that it adds
/// none.
fn segment_count(root: &Path) -> usize {
    tessera_store::read::open_bundle(root)
        .expect("the bundle opens")
        .partitions
        .values()
        .map(|p| p.manifest.segments.len())
        .sum()
}

fn session_of(engine: &Engine) -> Session {
    engine.authorise(&full_coverage_credential()).unwrap()
}

fn fields_of(engine: &Engine, session: &Session, entity: EntityId) -> BTreeMap<String, ScalarOut> {
    let id = engine.tessera_id_of(entity).unwrap();
    engine
        .item(session, id, None)
        .unwrap()
        .expect("the item is visible to a full principal")
        .fields
        .into_iter()
        .map(|f| (f.name, f.value))
        .collect()
}

fn leaf(column: &str, operand: FilterOperand) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.to_string(),
        operand,
    }
}

fn matching(engine: &Engine, session: &Session, filter: FilterExpr) -> Vec<u64> {
    let mut req = ViewportRequest::new("s0", 0, VIEWPORT, (N_ITEMS + 100) as usize);
    req.filter = Some(filter);
    engine
        .viewport(session, req)
        .unwrap()
        .points
        .iter()
        .map(|(id, _)| id.raw())
        .collect()
}

fn keyword(value: &str) -> FilterOperand {
    FilterOperand::TextEquals(value.to_string())
}

fn at_least(value: f64) -> FilterOperand {
    FilterOperand::Range {
        lo: Some(Endpoint {
            value: Scalar::Float(value),
            inclusive: true,
        }),
        hi: None,
    }
}

/// The entity whose row this file fills: ingested, then flushed, so every fill below lands on an
/// entity whose blob row is already in a durable extent — the case the per-column claimant read
/// exists for (`ingest.md` §1.4).
fn flushed_entity(engine: &Engine) -> EntityId {
    let entity = ingest(
        engine,
        "points-1",
        vec![row("subject", engine, build_columns("mid", 4.0))],
    )[0];
    flush(engine);
    entity
}

// ---------------------------------------------------------------------------------------------

/// **Every family fills on an entity that predates the value, and reads back.** The five columns
/// cover the three homes a value can have: `note` is blob-resident, `tag`, `dept` and `weight`
/// take entity-space value columns, and `prose` a text layer. Each is read back through the
/// surface that answers it — the filter for the indexed ones, the drill-down for all of them.
#[test]
fn every_family_fills_on_an_entity_that_predates_the_value_and_reads_back() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    let entity = flushed_entity(&engine);
    let session = session_of(&engine);

    // Before the fill: the entity carries none of the five.
    let before = fields_of(&engine, &session, entity);
    for family in FAMILIES {
        assert!(
            !before.contains_key(family),
            "'{family}' is absent before the fill, and reads {:?}",
            before.get(family)
        );
    }

    let receipt = engine
        .fill_values(values_request(
            "values-1",
            &FAMILIES,
            vec![(entity, filled_values())],
        ))
        .expect("the fill is accepted");
    assert_eq!(receipt.filled, 5, "one cell per family");
    assert_eq!(receipt.held, 0);
    flush(&engine);

    let session = session_of(&engine);
    let after = fields_of(&engine, &session, entity);
    assert_eq!(
        after.get("note"),
        Some(&ScalarOut::Utf8("a private note".to_string())),
        "the blob-resident cell reads back: {after:?}"
    );
    assert_eq!(
        after.get("tag"),
        Some(&ScalarOut::Utf8("alpha".to_string())),
        "the indexed keyword reads back: {after:?}"
    );
    assert_eq!(
        after.get("dept"),
        Some(&ScalarOut::Utf8("ops".to_string())),
        "the category reads back by key: {after:?}"
    );
    assert_eq!(
        after.get("weight"),
        Some(&ScalarOut::F32(12.5)),
        "the indexed scalar reads back: {after:?}"
    );

    // The filter answers over the filled cells, in entity space, for every indexed family.
    let id = engine.tessera_id_of(entity).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("alpha"))),
        vec![id],
        "the indexed keyword filters"
    );
    assert_eq!(
        matching(
            &engine,
            &session,
            leaf(
                "dept",
                FilterOperand::Equals(tessera_types::AttrLocalId::new(6))
            )
        ),
        vec![id],
        "the category filters over the code the fill wrote"
    );
    assert_eq!(
        matching(&engine, &session, leaf("weight", at_least(12.0))),
        vec![id],
        "the indexed scalar filters over the cell the fill wrote"
    );

    // **A `render` column cannot be filled** (`ingest.md` §6.3). Its value is served from the hot
    // column of the row that carries it, and a values row acquires no row — so the value has
    // nowhere to land that any reader would answer from, and the batch is refused rather than
    // acknowledged having stored nothing.
    //
    // **Only a build declares one.** `PUT /control/attributes` refuses `render` as an interim
    // (decision 0136's amendment), so this guard is unreachable for a column declared at a running
    // service and fires for the fixture's build columns: `band`, rendered and not indexed, which
    // has no home at all for a fill, and `score`, rendered and indexed, which has an entity-space
    // column that would answer one filter route and nothing else.
    let refused_declaration = engine
        .declare_attribute(AttributeRequest {
            name: "drawn".to_string(),
            title: None,
            ty: "f32".to_string(),
            vocabulary: None,
            analyser: None,
            index: false,
            render: true,
            scope: LayerScope::Entity,
        })
        .expect_err("`render` is not declarable at a running service");
    assert!(matches!(
        refused_declaration,
        AcceptError::Exec(ExecError::AttributeRefused { .. })
    ));
    for column in ["band", "score"] {
        let refused = engine
            .fill_values(values_request(
                "values-2",
                &[column],
                vec![(entity, vec![WalScalar::U8(1)])],
            ))
            .expect_err("a rendered column is refused");
        let AcceptError::Exec(ExecError::ValuesRefused { detail }) = refused else {
            panic!("a row-tail-only column is a ValuesRefused, not {refused:?}");
        };
        assert!(
            detail.contains(&format!("column '{column}'")) && detail.contains("`render`"),
            "the refusal names the column and why: {detail}"
        );
    }
    assert_eq!(
        matching(
            &engine,
            &session,
            leaf(
                "prose",
                FilterOperand::Match {
                    query: "brown".to_string(),
                    minimum: None,
                }
            )
        ),
        vec![id],
        "the text layer the fill wrote answers a match"
    );
}

/// **The fill rule's other two arms** (`ingest.md` §1.1): a cell restated with the value it holds
/// is accepted with no effect, and one supplied with a different value refuses the whole batch
/// with a `409` naming the column — and never the held value.
#[test]
fn an_identical_value_is_a_no_op_and_a_different_one_is_refused() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    let entity = flushed_entity(&engine);

    engine
        .fill_values(values_request(
            "values-1",
            &FAMILIES,
            vec![(entity, filled_values())],
        ))
        .expect("the first fill is accepted");
    flush(&engine);

    // Restated identically, under a fresh batch id so nothing is answered as a replay.
    let again = engine
        .fill_values(values_request(
            "values-2",
            &["tag", "dept", "weight"],
            vec![(
                entity,
                vec![
                    WalScalar::Utf8("alpha".to_string()),
                    WalScalar::U8(6),
                    WalScalar::F32(12.5),
                ],
            )],
        ))
        .expect("a restatement is accepted");
    assert_eq!(again.filled, 0, "a restatement fills nothing");
    assert_eq!(again.held, 3, "and is counted as held");

    let refused = engine
        .fill_values(values_request(
            "values-3",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("beta".to_string())])],
        ))
        .expect_err("a different value is refused");
    let AcceptError::Exec(ExecError::ValueConflict { detail }) = refused else {
        panic!("a held cell supplied differently is a ValueConflict, not {refused:?}");
    };
    assert!(
        detail.contains("column 'tag'"),
        "the refusal names the column: {detail}"
    );
    assert!(
        !detail.contains("alpha") && !detail.contains("beta"),
        "the refusal names neither value (`ingest.md` §1.4): {detail}"
    );

    // And the refusal left nothing behind: the cell still holds what it held.
    let session = session_of(&engine);
    assert_eq!(
        fields_of(&engine, &session, entity).get("tag"),
        Some(&ScalarOut::Utf8("alpha".to_string()))
    );
}

/// **The fill rule reads past a buffered row that holds the column's absence** (`ingest.md`
/// §1.1). An entity ingested without a column carries the position and holds absence in it, so a
/// source chain that stopped at the first row *carrying the slot* would never reach the pending
/// fill or the flushed home: the second fill of one cell would read as absent, be answered `200
/// filled 1`, and then be dropped at the merge. Both arms are exercised while the entity is still
/// buffered, which is where the absent slot exists.
#[test]
fn a_buffered_row_holding_an_absent_cell_does_not_hide_a_pending_fill() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    // Ingested and **not** flushed: the entity's own row is in the buffer, carrying every
    // declared position and holding absence in the runtime ones.
    let entity = ingest(
        &engine,
        "points-1",
        vec![row("subject", &engine, build_columns("mid", 4.0))],
    )[0];

    let first = engine
        .fill_values(values_request(
            "values-1",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("alpha".to_string())])],
        ))
        .expect("the first fill is accepted");
    assert_eq!(first.filled, 1);

    // The buffered row still holds the column's absence, and the pending fill holds the value.
    let refused = engine
        .fill_values(values_request(
            "values-2",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("beta".to_string())])],
        ))
        .expect_err("the pending fill is a claimant, so a different value is refused");
    let AcceptError::Exec(ExecError::ValueConflict { detail }) = refused else {
        panic!("a cell an unflushed fill holds is a ValueConflict, not {refused:?}");
    };
    assert!(detail.contains("column 'tag'"), "{detail}");

    // And the identical value is the no-op, not a second fill of one cell.
    let again = engine
        .fill_values(values_request(
            "values-3",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("alpha".to_string())])],
        ))
        .expect("a restatement is accepted");
    assert_eq!(again.filled, 0, "a restatement fills nothing");
    assert_eq!(again.held, 1);

    // One claimant reaches the extent, so the value the first fill supplied is the one served.
    flush(&engine);
    let session = session_of(&engine);
    let id = engine.tessera_id_of(entity).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("alpha"))),
        vec![id]
    );
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("beta"))),
        Vec::<u64>::new(),
        "the refused value reached nothing"
    );
}

/// **A buffered row that holds a value is a claimant too** — the same arm, from the other side:
/// a batch that ingested the column and a values batch that supplies a different one for it
/// disagree, and the fill rule refuses rather than writing a second cell.
#[test]
fn a_buffered_row_holding_a_value_refuses_a_different_fill_and_dedupes_an_equal_one() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    let entity = ingest(
        &engine,
        "points-1",
        vec![{
            let mut row = row("subject", &engine, build_columns("mid", 4.0));
            // note, tag, prose, dept, weight, drawn — `tag` carried, the rest absent.
            row.scalars.push(WalScalar::Null);
            row.scalars.push(WalScalar::Utf8("alpha".to_string()));
            row
        }],
    )[0];

    let refused = engine
        .fill_values(values_request(
            "values-1",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("beta".to_string())])],
        ))
        .expect_err("the buffered row holds the cell");
    assert!(matches!(
        refused,
        AcceptError::Exec(ExecError::ValueConflict { .. })
    ));

    let again = engine
        .fill_values(values_request(
            "values-2",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("alpha".to_string())])],
        ))
        .expect("a restatement is accepted");
    assert_eq!(again.filled, 0);
    assert_eq!(again.held, 1);

    // The buffered row is the one claimant: the flush writes one slot, not two.
    flush(&engine);
    let session = session_of(&engine);
    let id = engine.tessera_id_of(entity).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("alpha"))),
        vec![id]
    );
}

/// **A fill on an entity whose blob row has already flushed is read** (`ingest.md` §1.4, decision
/// 0136 ruling 7). The entity holds a row in the layer that created it and another in the layer
/// the fill wrote, so a first-layer-wins probe would answer the first row alone and the filled
/// column would read absent. The claimant read unions the two, and the cost is one block decode
/// per claimant.
#[test]
fn a_fill_on_a_flushed_entity_is_read_through_the_claimant_read() {
    let fx = fixture();
    let engine = engine_over(&fx);
    // A blob-resident column the *build's* rows already carry, so the entity's first flush writes
    // it a blob row and the fill writes a second one in another layer.
    declare(&engine, "origin", "keyword", false);
    declare(&engine, "note", "keyword", false);
    let entity = ingest(
        &engine,
        "points-1",
        vec![{
            let mut row = row("subject", &engine, build_columns("mid", 4.0));
            row.scalars.push(WalScalar::Utf8("first layer".to_string()));
            row.scalars.push(WalScalar::Null);
            row
        }],
    )[0];
    flush(&engine);

    let session = session_of(&engine);
    let before = fields_of(&engine, &session, entity);
    assert_eq!(
        before.get("origin"),
        Some(&ScalarOut::Utf8("first layer".to_string())),
        "the creating layer's blob row is read"
    );

    engine
        .fill_values(values_request(
            "values-1",
            &["note"],
            vec![(entity, vec![WalScalar::Utf8("second layer".to_string())])],
        ))
        .expect("the fill is accepted");
    flush(&engine);

    let session = session_of(&engine);
    let after = fields_of(&engine, &session, entity);
    assert_eq!(
        after.get("origin"),
        Some(&ScalarOut::Utf8("first layer".to_string())),
        "the creating layer's column is still read: {after:?}"
    );
    assert_eq!(
        after.get("note"),
        Some(&ScalarOut::Utf8("second layer".to_string())),
        "and the filling layer's column beside it: {after:?}"
    );

    // The fold merges the two rows into one, and both columns are still read.
    fold(&engine);
    let session = session_of(&engine);
    let folded = fields_of(&engine, &session, entity);
    assert_eq!(folded.get("origin"), after.get("origin"), "after the fold: {folded:?}");
    assert_eq!(folded.get("note"), after.get("note"), "after the fold: {folded:?}");
}

/// **A tick whose only work is fills publishes them, and writes no segment** (`ingest.md` §1.4).
/// A fill acquires no geometry, so there is no row for a segment to hold; the value extents and
/// the record blob's layer are published without one, which is what makes a fill visible at the
/// tick on a corpus that is not also ingesting.
#[test]
fn a_values_only_tick_publishes_the_cells_with_no_new_segment() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    let entity = flushed_entity(&engine);
    let segments = segment_count(&fx.root);

    engine
        .fill_values(values_request(
            "values-1",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("alpha".to_string())])],
        ))
        .expect("the fill is accepted");
    flush(&engine);

    assert_eq!(
        segment_count(&fx.root),
        segments,
        "a values-only tick adds no segment"
    );
    let session = session_of(&engine);
    let id = engine.tessera_id_of(entity).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("alpha"))),
        vec![id],
        "and the cell it published answers a filter"
    );
}

/// A fill whose entity is deleted before the next tick does not stay in the buffer: a fill holds
/// the log at its position, so one that no flush will ever consume would stop rotation for good.
#[test]
fn a_fill_on_an_entity_deleted_before_the_tick_does_not_hold_the_log() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    let entity = flushed_entity(&engine);

    engine
        .fill_values(values_request(
            "values-1",
            &["tag"],
            vec![(entity, vec![WalScalar::Utf8("alpha".to_string())])],
        ))
        .expect("the fill is accepted");
    engine
        .accept_change(entity, tessera_lifecycle::ChangeOp::Delete)
        .expect("the delete is accepted");
    settle(&engine);

    assert_eq!(
        engine.generation().buffer.oldest_wal_pos(),
        None,
        "nothing buffered holds the log"
    );

    let engine = restart(&fx, engine);
    assert_eq!(
        engine.generation().buffer.oldest_wal_pos(),
        None,
        "and a restart does not buffer the fill again"
    );
}

/// **The fold folds the filling layer into the base** (`ingest.md` §1.4). The record blob's
/// layers become one row again and the entity-space extents one column, so every filled cell is
/// still read afterwards — through one claimant rather than two.
#[test]
fn the_fold_carries_a_filled_cell_forward() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    let entity = flushed_entity(&engine);

    engine
        .fill_values(values_request(
            "values-1",
            &FAMILIES,
            vec![(entity, filled_values())],
        ))
        .expect("the fill is accepted");
    flush(&engine);
    fold(&engine);

    let session = session_of(&engine);
    let after = fields_of(&engine, &session, entity);
    assert_eq!(
        after.get("note"),
        Some(&ScalarOut::Utf8("a private note".to_string())),
        "the blob-resident cell survives the fold: {after:?}"
    );
    assert_eq!(
        after.get("tag"),
        Some(&ScalarOut::Utf8("alpha".to_string())),
        "and the indexed keyword: {after:?}"
    );
    let id = engine.tessera_id_of(entity).unwrap().raw();
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("alpha"))),
        vec![id],
        "and the filter still answers over it"
    );
}

/// **A restart replays a values batch, and the flush writes its cells once** (`ingest.md` §1.4).
/// The WAL member holding the record is reclaimed on its own schedule, so a restart re-buffers
/// fills a flush has already written; the plan drops every cell the flushed homes already hold,
/// which is what keeps one claimant per column.
#[test]
fn a_restart_replays_a_values_batch_and_writes_its_cells_once() {
    let fx = fixture();
    let engine = engine_over(&fx);
    declare_families(&engine);
    let entity = flushed_entity(&engine);

    engine
        .fill_values(values_request(
            "values-1",
            &FAMILIES,
            vec![(entity, filled_values())],
        ))
        .expect("the fill is accepted");

    // Restarted before the tick: the cells exist only in the log.
    let engine = restart(&fx, engine);
    let id = engine.tessera_id_of(entity).unwrap().raw();
    settle(&engine);
    let session = session_of(&engine);
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("alpha"))),
        vec![id],
        "the replayed fill is written by the flush after the restart"
    );
    assert_eq!(
        fields_of(&engine, &session, entity).get("note"),
        Some(&ScalarOut::Utf8("a private note".to_string()))
    );

    assert_eq!(
        engine.buffered_fills(),
        0,
        "the flush consumed the fills it wrote"
    );

    // Restarted again. Whether the record is still in the log is the rotation's business — the
    // fill released its pin above, so the member may already be reclaimed — and either way the
    // tick that follows must leave no fill behind: one re-buffered after its own flush writes
    // nothing, and a fill that is never consumed pins its `ValuesBatch` record for ever.
    let engine = restart(&fx, engine);
    settle(&engine);
    assert_eq!(
        engine.buffered_fills(),
        0,
        "a replayed fill whose cells are written is consumed rather than left pinning the log"
    );
    let session = session_of(&engine);
    assert_eq!(
        matching(&engine, &session, leaf("tag", keyword("alpha"))),
        vec![id],
        "and a second replay leaves one claimant per column"
    );
    assert_eq!(
        fields_of(&engine, &session, entity).get("note"),
        Some(&ScalarOut::Utf8("a private note".to_string()))
    );
}
