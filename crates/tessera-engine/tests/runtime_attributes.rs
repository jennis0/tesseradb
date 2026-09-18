//! **An attribute column declared while the service runs** (`ingest.md` §1.3, §6.3; decision
//! 0136, track T4): every family declares, rows carry it from the acknowledgement, an entity
//! that predates the declaration reads absent on every reader without a record-blob read, a
//! declaration between a batch's admission and its window close pads the rows rather than
//! failing the flush, a restart replays the declaration, the fold writes the column into the
//! base, and a redeclaration answers the column that exists or refuses a different identity.
//!
//! The fixture declares two columns at the build (a rendered `u8` category and a rendered,
//! indexed `f32`), so every case runs over a schema in which the build's columns and the runtime
//! ones are one list, and the runtime ones append after positions the build already filled.

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
    AcceptError, AttributeRequest, CategoryQuery, Engine, ScalarOut, Session, ViewportRequest,
};
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::ExecError;
use tessera_store::read::open_bundle;
use tessera_types::layer::LayerScope;
use tessera_types::{AttrLocalId, EntityId};

const N_ITEMS: u64 = 120;
const VIEWPORT: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// A rendered `u8` category and a rendered, indexed `f32`; a second vocabulary, `dept`, that no
/// build column names, so a runtime category over it fixes its width at the declaration.
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
visibility = "derived"
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

fn score_of(entity: u64) -> f32 {
    (entity % 97) as f32 * 0.5
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
    let scores: Vec<f32> = ids.iter().map(|e| score_of(*e)).collect();
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

fn build_fixture_with_schema(root: &Path, tmp: &Path) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points(&points, N_ITEMS);
    write_pairs_n(&pairs, N_ITEMS);
    let schema_path = tmp.join("schema.toml");
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
        out: root.to_path_buf(),
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
}

/// The fixture on disc: a bundle root and the tempdir it lives in.
struct Fixture {
    tmp: tempfile::TempDir,
    root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_schema(&root, tmp.path());
    Fixture { tmp, root }
}

/// An engine over the fixture with the executor running and the background refresh off, for
/// `tests/fold.rs`'s reason: a pass after a publication rebuilds each resident session's
/// fragment, which would make a check that failed to notice a publication indistinguishable from
/// one that noticed.
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

/// Reopen the same bundle and the same log, which is the restart every durability claim below
/// is made against. The old engine is dropped first so its executor releases the log.
fn restart(fx: &Fixture, engine: Engine) -> Engine {
    drop(engine);
    engine_over(fx)
}

fn flush(engine: &Engine) {
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes == before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, before.fold_failures,
            "the fold was discarded rather than published"
        );
        if now.folds > before.folds {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn request(name: &str, ty: &str) -> AttributeRequest {
    AttributeRequest {
        name: name.to_string(),
        title: None,
        ty: ty.to_string(),
        vocabulary: None,
        analyser: None,
        index: false,
        render: false,
        scope: LayerScope::Entity,
    }
}

/// One row carrying the build's two columns and nothing else, at the arity the build declared,
/// under the label every principal of the fixture holds.
fn row(external_id: &str, terms: &Engine, scalars: Vec<WalScalar>) -> UnallocatedRow {
    row_under(external_id, terms, b"0", scalars)
}

/// [`row`] under one label of the caller's choosing.
fn row_under(
    external_id: &str,
    terms: &Engine,
    label: &[u8],
    scalars: Vec<WalScalar>,
) -> UnallocatedRow {
    UnallocatedRow {
        external_id: Some(external_id.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![label.to_vec()],
        x: 500.0,
        y: 500.0,
        scalars,
        terms: terms.resolve_terms(&[label.to_vec()]),
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

fn session(engine: &Engine) -> Session {
    engine.authorise(&full_coverage_credential()).unwrap()
}

fn viewport(engine: &Engine, session: &Session, filter: Option<FilterExpr>) -> ViewportOut {
    let mut req = ViewportRequest::new("s0", 0, VIEWPORT, (N_ITEMS + 100) as usize);
    req.filter = filter;
    let out = engine.viewport(session, req).unwrap();
    ViewportOut {
        ids: out.points.iter().map(|(id, _)| id.raw()).collect(),
        names: out.scalar_names.clone(),
    }
}

struct ViewportOut {
    ids: Vec<u64>,
    names: Vec<String>,
}

fn leaf(column: &str, operand: FilterOperand) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.to_string(),
        operand,
    }
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

fn declared_names(engine: &Engine) -> Vec<String> {
    engine
        .meta()
        .declared_scalars
        .iter()
        .map(|d| d.name.clone())
        .collect()
}

/// The entity id the build gave source row 0, through the external-id map the sidecar holds.
fn built_entity(root: &Path, source: u64) -> EntityId {
    EntityId::new(source_to_new_map(root, "v00000")[&source])
}

/// How many of the prefix's partitions hold an entity-space base value file for `column`, over the
/// number of partitions. A column declared at a running service has no base until a fold writes one
/// (`ingest.md` §6.3), so this is 0 before the fold and every partition after it.
fn partitions_with_base(root: &Path, column: &str) -> (usize, usize) {
    let bundle = open_bundle(root).expect("the bundle opens");
    let current: tessera_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).unwrap()).unwrap();
    let (mut carrying, mut total) = (0, 0);
    for phash in bundle.partitions.keys() {
        total += 1;
        if root
            .join(&current.prefix)
            .join("partitions")
            .join(phash)
            .join("attrs")
            .join(column)
            .join(tessera_filter::VALUES_FILE)
            .is_file()
        {
            carrying += 1;
        }
    }
    (carrying, total)
}

// ---------------------------------------------------------------------------------------------

/// **Every family declares at a running service, rows carry it from the acknowledgement, and an
/// entity that predates the declaration reads absent on every reader.** The absence is answered
/// from the segment schema: the record-blob read counter does not move for the older entity,
/// while it does for one whose row the flush wrote into the blob. The render list is the build's
/// throughout: this route does not accept `render` (decision 0136's amendment).
#[test]
fn every_family_declares_at_runtime_and_earlier_entities_read_absent_without_a_blob_read() {
    let fx = fixture();
    let engine = engine_over(&fx);

    let declared = [
        AttributeRequest {
            index: true,
            ..request("sentiment", "f32")
        },
        AttributeRequest {
            index: true,
            ..request("note", "keyword")
        },
        AttributeRequest {
            index: true,
            ..request("prose", "text")
        },
        AttributeRequest {
            vocabulary: Some("dept".to_string()),
            index: true,
            ..request("tag", "category")
        },
        // Neither flag: blob-resident, served at drill-down alone (records §3).
        request("memo", "u16"),
    ];
    for r in declared {
        assert!(
            !engine
                .declare_attribute(r.clone())
                .expect("the declaration is accepted"),
            "'{}' is a new column",
            r.name
        );
    }
    assert_eq!(
        declared_names(&engine),
        ["band", "score", "sentiment", "note", "prose", "tag", "memo"],
        "the runtime columns append after the build's, in declaration order"
    );
    let meta = engine.meta();
    let tag = meta
        .declared_scalars
        .iter()
        .find(|d| d.name == "tag")
        .unwrap();
    assert_eq!(tag.arrow_type, tessera_engine::ScalarType::U8);
    assert_eq!(tag.vocabulary.as_deref(), Some("dept"));

    // Rows carrying every column, ingestable at the ack.
    let full = |id: &str, sentiment: f32, note: &str, prose: &str, tag: &str, memo: u16| {
        let mut scalars = build_columns("mid", 1.0);
        scalars.extend([
            WalScalar::F32(sentiment),
            WalScalar::Utf8(note.to_string()),
            WalScalar::Utf8(prose.to_string()),
            WalScalar::Utf8(tag.to_string()),
            WalScalar::U16(memo),
        ]);
        row(id, &engine, scalars)
    };
    let new = ingest(
        &engine,
        "carrying",
        vec![
            full("n1", 0.9, "alpha", "the quick brown fox", "eng", 7),
            full("n2", 0.2, "beta", "a slow red hen", "ops", 8),
            full("n3", 0.7, "alpha", "the quick grey wolf", "eng", 9),
        ],
    );
    flush(&engine);

    let session = session(&engine);
    let out = viewport(&engine, &session, None);
    assert_eq!(
        out.names,
        ["band", "score"],
        "the render tail is the build's alone: this route does not accept `render`"
    );
    let old = built_entity(&fx.root, 0);

    // The drill-down: absent for the older entity, from the schema, with no blob read.
    let reads_before = engine.generation().filter_columns.record_reads();
    let old_fields = fields_of(&engine, &session, old);
    assert_eq!(
        old_fields.keys().cloned().collect::<Vec<_>>(),
        ["band", "score"],
        "an entity that predates the declaration carries none of the new columns"
    );
    assert_eq!(
        engine.generation().filter_columns.record_reads(),
        reads_before,
        "absence is answered from the segment schema, never by opening a blob"
    );
    let new_fields = fields_of(&engine, &session, new[0]);
    assert_eq!(new_fields["sentiment"], ScalarOut::F32(0.9));
    assert_eq!(new_fields["note"], ScalarOut::Utf8("alpha".to_string()));
    assert_eq!(
        new_fields["prose"],
        ScalarOut::Utf8("the quick brown fox".to_string())
    );
    assert_eq!(new_fields["tag"], ScalarOut::Utf8("eng".to_string()));
    assert_eq!(new_fields["memo"], ScalarOut::U16(7));
    assert!(
        engine.generation().filter_columns.record_reads() > reads_before,
        "the entity whose row the flush wrote is read from the blob"
    );

    // The filter, per family, over the ingested rows.
    let ids_of = |entities: &[EntityId]| -> Vec<u64> {
        let mut ids: Vec<u64> = entities
            .iter()
            .map(|e| engine.tessera_id_of(*e).unwrap().raw())
            .collect();
        ids.sort_unstable();
        ids
    };
    let sorted = |mut ids: Vec<u64>| {
        ids.sort_unstable();
        ids
    };
    assert_eq!(
        sorted(viewport(&engine, &session, Some(leaf("sentiment", at_least(0.5)))).ids),
        ids_of(&[new[0], new[2]])
    );
    assert_eq!(
        sorted(
            viewport(
                &engine,
                &session,
                Some(leaf("note", FilterOperand::TextEquals("alpha".to_string())))
            )
            .ids
        ),
        ids_of(&[new[0], new[2]])
    );
    assert_eq!(
        sorted(
            viewport(
                &engine,
                &session,
                Some(leaf(
                    "prose",
                    FilterOperand::Match {
                        query: "quick".to_string(),
                        minimum: None,
                    }
                ))
            )
            .ids
        ),
        ids_of(&[new[0], new[2]])
    );
    assert_eq!(
        sorted(
            viewport(
                &engine,
                &session,
                Some(leaf("tag", FilterOperand::Equals(AttrLocalId::new(6))))
            )
            .ids
        ),
        ids_of(&[new[1]])
    );

    // The categories vocabulary of a `derived` runtime category: the values a visible member
    // carries, from the extents alone, since the column has no base postings before the fold.
    let page = engine
        .categories(
            &session,
            "tag",
            CategoryQuery::Page {
                after: None,
                limit: 10,
            },
        )
        .unwrap()
        .expect("a runtime category column answers");
    let keys: Vec<&str> = page.values.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(
        keys,
        ["eng", "ops"],
        "only the values an ingested row carries"
    );

    // And the suggest verb, whose index the declaration built for a vocabulary no build column
    // named: a value a visible member carries is offered, one nothing carries is not.
    let suggested = engine
        .suggest(&session, "tag", "e", 20, false, 100_000, 0)
        .expect("a runtime category's vocabulary has a suggestion index")
        .expect("the column answers");
    let keys: Vec<&str> = suggested.values.iter().map(|v| v.key.as_str()).collect();
    assert_eq!(keys, ["eng"]);
}

/// **A declaration between a batch's admission and its window close pads the rows** (`ingest.md`
/// §7.1): a batch admitted at the earlier arity closes and flushes under the wider schema with
/// the new column absent, and a batch decoded against the earlier schema after the declaration
/// is accepted the same way. A row longer than the schema is still refused.
#[test]
fn a_declaration_mid_ingest_pads_earlier_rows_and_neither_panics_nor_fails_the_flush() {
    let fx = fixture();
    let engine = engine_over(&fx);

    // Admitted and closed under the build's arity, buffered for the flush.
    let before = ingest(
        &engine,
        "before",
        vec![row("b1", &engine, build_columns("low", 2.0))],
    );
    engine
        .declare_attribute(AttributeRequest {
            index: true,
            ..request("late", "f32")
        })
        .expect("the declaration is accepted");
    // Decoded against the earlier schema: shorter by one, padded at its window's close.
    let after = ingest(
        &engine,
        "after-short",
        vec![row("a1", &engine, build_columns("mid", 3.0))],
    );
    // And a row longer than the schema is refused before anything is admitted.
    let mut long = build_columns("high", 4.0);
    long.extend([WalScalar::F32(1.0), WalScalar::F32(2.0)]);
    let refused = engine
        .accept_ingest(
            vec![row("too-long", &engine, long)],
            "after-long".to_string(),
            [7u8; 32],
        )
        .expect_err("a row longer than the schema is refused");
    assert!(
        matches!(
            refused,
            AcceptError::ScalarArity {
                got: 4,
                expected: 3,
                ..
            }
        ),
        "{refused}"
    );
    let carrying = ingest(
        &engine,
        "after-full",
        vec![row("c1", &engine, {
            let mut s = build_columns("high", 4.0);
            s.push(WalScalar::F32(0.75));
            s
        })],
    );

    flush(&engine);

    let session = session(&engine);
    let id = |e: EntityId| engine.tessera_id_of(e).unwrap().raw();
    assert_eq!(
        fields_of(&engine, &session, carrying[0])["late"],
        ScalarOut::F32(0.75)
    );
    assert!(
        !fields_of(&engine, &session, before[0]).contains_key("late"),
        "a padded row's absence is an absence, not a zero"
    );
    assert!(
        !fields_of(&engine, &session, after[0]).contains_key("late"),
        "a row decoded against the earlier schema is padded the same way"
    );
    assert_eq!(
        viewport(&engine, &session, Some(leaf("late", at_least(0.0)))).ids,
        vec![id(carrying[0])],
        "a filter over the column matches the row that carried it and no padded row"
    );
}

/// **A restart replays the declaration**, from the log alone before any publication and from
/// the segments manifest after one; and the log holding the declaration beside a batch admitted
/// under the earlier arity flushes after the restart.
#[test]
fn a_restart_replays_the_declaration_from_the_log_and_from_the_manifest() {
    let fx = fixture();
    let engine = engine_over(&fx);
    let before = ingest(
        &engine,
        "before",
        vec![row("b1", &engine, build_columns("low", 2.0))],
    );
    engine
        .declare_attribute(AttributeRequest {
            index: true,
            ..request("sentiment", "f32")
        })
        .expect("the declaration is accepted");
    // Nothing published: the declaration is in the log alone.
    let engine = restart(&fx, engine);
    assert_eq!(declared_names(&engine), ["band", "score", "sentiment"]);
    assert!(
        engine
            .declare_attribute(AttributeRequest {
                index: true,
                ..request("sentiment", "f32")
            })
            .expect("an identical redeclaration is accepted"),
        "the replayed column is the one the redeclaration meets"
    );
    let carrying = ingest(
        &engine,
        "carrying",
        vec![row("c1", &engine, {
            let mut s = build_columns("mid", 1.0);
            s.push(WalScalar::F32(0.6));
            s
        })],
    );
    flush(&engine);
    let side = open_bundle(&fx.root).unwrap();
    let published: Vec<&str> = side
        .partitions
        .values()
        .flat_map(|p| p.manifest.attributes.iter().map(|d| d.name.as_str()))
        .collect();
    assert_eq!(
        published,
        ["sentiment"],
        "the segments manifest is the durable home"
    );

    // Published: the manifest carries it, and the log may not.
    let engine = restart(&fx, engine);
    assert_eq!(declared_names(&engine), ["band", "score", "sentiment"]);
    let session = session(&engine);
    let id = |e: EntityId| engine.tessera_id_of(e).unwrap().raw();
    assert!(!fields_of(&engine, &session, before[0]).contains_key("sentiment"));
    assert_eq!(
        fields_of(&engine, &session, carrying[0])["sentiment"],
        ScalarOut::F32(0.6)
    );
    assert_eq!(
        viewport(&engine, &session, Some(leaf("sentiment", at_least(0.5)))).ids,
        vec![id(carrying[0])]
    );
}

/// **The fold materialises the column into the base**: every segment of the new prefix carries
/// it, the new `MANIFEST.json` declares it beside the build's columns, the segments manifest's
/// runtime list is empty, and the reopened prefix serves and filters it.
#[test]
fn the_fold_carries_a_runtime_column_into_the_base() {
    let fx = fixture();
    let engine = engine_over(&fx);
    engine
        .declare_attribute(AttributeRequest {
            index: true,
            ..request("sentiment", "f32")
        })
        .expect("the declaration is accepted");
    // A second runtime column no flush ever carries, so the fold writes it an empty base.
    engine
        .declare_attribute(AttributeRequest {
            index: true,
            ..request("never", "i64")
        })
        .expect("the declaration is accepted");
    let carrying = ingest(
        &engine,
        "carrying",
        vec![row("c1", &engine, {
            let mut s = build_columns("mid", 1.0);
            s.extend([WalScalar::F32(0.6), WalScalar::Null]);
            s
        })],
    );
    flush(&engine);
    assert_eq!(
        partitions_with_base(&fx.root, "sentiment"),
        (0, 1),
        "before the fold the column has no base and its stack is the flush's extents alone"
    );

    fold(&engine);
    assert_eq!(engine.generation().prefix, "v00001");
    assert_eq!(
        partitions_with_base(&fx.root, "sentiment"),
        (1, 1),
        "the fold wrote the column's base"
    );
    let folded = open_bundle(&fx.root).unwrap();
    assert_eq!(
        folded
            .manifest
            .declared_scalars
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["band", "score", "sentiment", "never"],
        "the new MANIFEST.json declares the runtime columns after the build's"
    );
    assert!(
        folded
            .partitions
            .values()
            .all(|p| p.manifest.attributes.is_empty()),
        "the runtime list is empty once the fold has written the columns"
    );

    let session = session(&engine);
    let id = engine.tessera_id_of(carrying[0]).unwrap().raw();
    assert_eq!(
        fields_of(&engine, &session, carrying[0])["sentiment"],
        ScalarOut::F32(0.6)
    );
    assert_eq!(
        viewport(&engine, &session, Some(leaf("sentiment", at_least(0.5)))).ids,
        vec![id],
        "the folded base column answers the filter"
    );
    assert!(
        viewport(
            &engine,
            &session,
            Some(leaf("never", FilterOperand::Range { lo: None, hi: None }))
        )
        .ids
        .is_empty(),
        "a column no row carried folds to an empty base"
    );

    // And the folded prefix reopens, with the declarations now the build's.
    let engine = restart(&fx, engine);
    assert_eq!(
        declared_names(&engine),
        ["band", "score", "sentiment", "never"]
    );
    let session = self::session(&engine);
    assert_eq!(
        viewport(&engine, &session, Some(leaf("sentiment", at_least(0.5)))).ids,
        vec![id]
    );
}

/// **A runtime category's vocabulary is offered from inside each principal's mask** (I2,
/// per-point-attributes §3.3): each principal is offered the values its own visible rows carry
/// and nothing a row outside its mask carries. The fixture's two principals hold one term each
/// (`common::terms_of`), so a row under either label is visible to one of them only. The column
/// has no base postings before the fold, so the answer comes from the extents alone.
#[test]
fn a_runtime_category_offers_a_restricted_principal_only_its_visible_values() {
    let fx = fixture();
    let engine = engine_over(&fx);
    engine
        .declare_attribute(AttributeRequest {
            vocabulary: Some("dept".to_string()),
            index: true,
            ..request("tag", "category")
        })
        .expect("the declaration is accepted");
    let tagged = |id: &str, label: &[u8], tag: &str| {
        let mut scalars = build_columns("mid", 1.0);
        scalars.push(WalScalar::Utf8(tag.to_string()));
        row_under(id, &engine, label, scalars)
    };
    // `eng` only on a row under label 0, which the principal holding term 1 cannot see; `ops`
    // only on a row under label 1, which the principal holding term 0 cannot.
    ingest(
        &engine,
        "labelled",
        vec![tagged("e1", b"0", "eng"), tagged("o1", b"1", "ops")],
    );
    flush(&engine);

    let offered = |credential: &[u8]| -> Vec<String> {
        let session = engine.authorise(credential).unwrap();
        engine
            .categories(
                &session,
                "tag",
                CategoryQuery::Page {
                    after: None,
                    limit: 10,
                },
            )
            .unwrap()
            .expect("the column answers")
            .values
            .into_iter()
            .map(|v| v.key)
            .collect()
    };
    assert_eq!(
        offered(&full_coverage_credential()),
        ["eng"],
        "a value carried only by rows outside the principal's mask is not offered"
    );
    assert_eq!(offered(&subset_credential()), ["ops"]);
}

/// **A declaration made while a fold is in flight survives the publication at the same tail
/// position** (`ingest.md` §6.3): the fold's `MANIFEST.json` carries the schema as it stood at
/// the plan, the side manifest carries the declaration made since, and the reopen appends it
/// after the folded columns, which is where every row buffered under it holds its value.
#[test]
fn a_declaration_during_a_fold_survives_the_publication_at_the_same_tail_position() {
    let fx = fixture();
    let engine = engine_over(&fx);
    engine
        .declare_attribute(AttributeRequest {
            index: true,
            ..request("before", "f32")
        })
        .expect("the declaration is accepted");
    ingest(
        &engine,
        "before",
        vec![row("b1", &engine, {
            let mut s = build_columns("mid", 1.0);
            s.push(WalScalar::F32(0.25));
            s
        })],
    );
    flush(&engine);

    engine.set_fold_paused_for_test(true);
    let stats_before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !engine.fold_is_holding_for_test() {
        assert!(
            std::time::Instant::now() < deadline,
            "the fold's passes never finished"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Declared while the fold holds: after `before`, at position 3.
    engine
        .declare_attribute(AttributeRequest {
            index: true,
            ..request("during", "f32")
        })
        .expect("a declaration during a fold is accepted");
    let during = ingest(
        &engine,
        "during",
        vec![row("d1", &engine, {
            let mut s = build_columns("high", 2.0);
            s.extend([WalScalar::F32(0.5), WalScalar::F32(0.75)]);
            s
        })],
    );
    engine.set_fold_paused_for_test(false);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let now = engine.write_executor_stats();
        assert_eq!(
            now.fold_failures, stats_before.fold_failures,
            "the fold published"
        );
        if now.folds > stats_before.folds {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the fold never published"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        declared_names(&engine),
        ["band", "score", "before", "during"],
        "the folded column keeps its place and the one declared during the fold follows it"
    );
    let folded = open_bundle(&fx.root).unwrap();
    assert_eq!(
        folded
            .manifest
            .declared_scalars
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        ["band", "score", "before"],
        "the fold's MANIFEST.json carries the schema as it stood at the plan"
    );
    let side: Vec<&str> = folded
        .partitions
        .values()
        .flat_map(|p| p.manifest.attributes.iter().map(|d| d.name.as_str()))
        .collect();
    assert_eq!(
        side,
        ["during"],
        "the declaration made during the fold is on the side manifest"
    );

    flush(&engine);
    let check = |engine: &Engine| {
        let session = session(engine);
        assert_eq!(
            viewport(engine, &session, None).names,
            ["band", "score"],
            "the render list is the build's alone"
        );
        let id = engine.tessera_id_of(during[0]).unwrap().raw();
        let fields = fields_of(engine, &session, during[0]);
        assert_eq!(
            fields["before"],
            ScalarOut::F32(0.5),
            "each value under its own name"
        );
        assert_eq!(fields["during"], ScalarOut::F32(0.75));
        assert_eq!(
            viewport(engine, &session, Some(leaf("during", at_least(0.7)))).ids,
            vec![id]
        );
    };
    check(&engine);
    let engine = restart(&fx, engine);
    assert_eq!(
        declared_names(&engine),
        ["band", "score", "before", "during"]
    );
    check(&engine);
}

/// **An identical redeclaration answers the column that exists; a differing one is a conflict;
/// a declaration the schema's rules refuse is refused** (`ingest.md` §1.1).
#[test]
fn an_identical_redeclaration_is_a_no_op_and_a_differing_one_conflicts() {
    let fx = fixture();
    let engine = engine_over(&fx);
    let sentiment = AttributeRequest {
        index: true,
        ..request("sentiment", "f32")
    };
    assert!(!engine.declare_attribute(sentiment.clone()).unwrap());
    assert!(
        engine.declare_attribute(sentiment.clone()).unwrap(),
        "identical: accepted with no effect"
    );
    assert_eq!(declared_names(&engine), ["band", "score", "sentiment"]);

    let conflict = |r: AttributeRequest| match engine.declare_attribute(r) {
        Err(AcceptError::Exec(ExecError::AttributeConflict { .. })) => {}
        other => panic!("a differing identity under a held name is a conflict: {other:?}"),
    };
    conflict(AttributeRequest {
        index: false,
        ..sentiment.clone()
    });
    conflict(AttributeRequest {
        ty: "f64".to_string(),
        ..sentiment.clone()
    });
    // The build's own column is a held name too.
    conflict(AttributeRequest {
        index: true,
        ..request("score", "f64")
    });
    assert!(
        engine
            .declare_attribute(AttributeRequest {
                index: true,
                ..request("sentiment", "f32")
            })
            .unwrap(),
        "restating a runtime column identically is the same no-op"
    );

    let refused = |r: AttributeRequest| match engine.declare_attribute(r) {
        Err(AcceptError::Exec(ExecError::AttributeRefused { detail })) => detail,
        other => panic!("refused at the door: {other:?}"),
    };
    assert!(refused(request("region", "u8")).contains("may not take it"));
    assert!(refused(request("weird", "utf8")).contains("retired"));
    assert!(refused(AttributeRequest {
        vocabulary: Some("nothing".to_string()),
        ..request("tag", "category")
    })
    .contains("no vocabulary named"));
    // **`render` is refused for every type, as an interim** (decision 0136's amendment): the
    // reason is that this route addresses entities rather than rows, so it does not depend on the
    // type, and the message says the refusal is not a rule about rendered columns.
    for r in [
        AttributeRequest {
            index: true,
            render: true,
            ..request("drawn", "f32")
        },
        AttributeRequest {
            vocabulary: Some("dept".to_string()),
            render: true,
            ..request("tag", "category")
        },
        AttributeRequest {
            vocabulary: Some("band".to_string()),
            render: true,
            ..request("band3", "category")
        },
        AttributeRequest {
            render: true,
            ..request("blurb", "text")
        },
    ] {
        let name = r.name.clone();
        let detail = refused(r);
        assert!(
            detail.contains("`render` is not accepted at a running service")
                && detail.contains("interim"),
            "'{name}': {detail}"
        );
    }
    // The build's own rendered column cannot be restated through this route either: the flag is
    // refused before the held-name comparison.
    assert!(refused(AttributeRequest {
        index: true,
        render: true,
        ..request("score", "f32")
    })
    .contains("`render` is not accepted at a running service"));
    assert!(refused(AttributeRequest {
        scope: LayerScope::Group("nowhere".to_string()),
        ..request("scoped", "i32")
    })
    .contains("does not declare"));
    assert_eq!(
        declared_names(&engine),
        ["band", "score", "sentiment"],
        "a refusal declares nothing"
    );
}
