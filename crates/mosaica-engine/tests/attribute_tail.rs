//! **The declared attribute tail survives every producer of `columns.arrow`.**
//!
//! Per-point-attributes §2's `render` placement puts a value in the hot row. Four things then write
//! that row — the batch build, a flush, a merge, and the compaction fold — and each takes its
//! writer schema from `MANIFEST.declared_scalars` rather than from the segment it is rewriting.
//! That indirection is what these cases exist for: a producer that read its schema from anywhere
//! else, or that dropped the tail because it had no opinion about it, would emit a segment whose
//! columns are *shorter* than the manifest declares — and the failure would not surface at the
//! write. It surfaces as every later row's value read under the wrong identity, or as a reader
//! refusing a bundle hours afterwards.
//!
//! **The assertion is always by `mosaica_id`, never by row.** Every one of these producers
//! legitimately reorders rows: a merge interleaves two segments in `(morton, mosaica_id)` order, a
//! fold rewrites the whole permutation. A row-indexed assertion would pass on a producer that
//! carried the values forward *unpermuted* — values present, every one against the wrong item —
//! which is precisely the defect that has no other symptom.
//!
//! What is **not** covered here, stated so its absence is not read as coverage: nothing asserts a
//! category *key* survives ingest, because ingest carries codes rather than keys (see
//! `an_ingested_row_carries_the_declared_tail_through_a_flush`).
//!
//! The file's second half is the same claim for the **third home**: a blob-resident column's
//! values survive every producer of the record blob — build, flush, coalesce and fold — read back
//! at the artefact level through `RecordStack`, which is how drill-down will read them. The
//! sharpest cases are write-path §5.4's two removal rules, asserted on the files: a suppression
//! (and an accepted-but-unfolded deletion) changes not one blob byte, and the fold that executes
//! a deletion leaves the row byte-absent.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use mosaica_build::config::{Config, Schema};
use mosaica_build::{build, BuildArgs};
use mosaica_engine::{ColumnBuf, Engine, EngineConfig, ViewportRequest};
use mosaica_lifecycle::command::UnallocatedRow;
use mosaica_lifecycle::wal::{ChangeOp, WalScalar};
use mosaica_store::read::{open_bundle, ColumnsRef, ScalarSlice};
use mosaica_types::{ItemHigh, Tenancy};

/// The fixture's schema: a `u8` category, a plain `i64` and a plain `f32`.
///
/// **Three widths, not one.** The tail is written and read back *positionally*, so a bug that
/// mixes up two columns is invisible in a schema whose columns are the same width — every value
/// lands somewhere legal. Different widths make a positional slip a type mismatch the readers
/// refuse, and make the `columns.arrow` byte size a check in its own right.
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

[[attribute]]
name       = "band"
type       = "category"
render     = true
vocabulary = "band"

[[attribute]]
name     = "ingested_at"
type     = "i64"
render = true

[[attribute]]
name     = "score"
type     = "f32"
render = true
"#;

/// The band key an entity carries in the fixture — three-way, so every declared code is exercised
/// and no code is the only one present.
fn band_of(entity: u64) -> &'static str {
    match entity % 3 {
        0 => "low",
        1 => "mid",
        _ => "high",
    }
}

fn band_code(entity: u64) -> u8 {
    match entity % 3 {
        0 => 1,
        1 => 2,
        _ => 3,
    }
}

fn ingested_at_of(entity: u64) -> i64 {
    1_700_000_000_000_000i64 + entity as i64
}

fn score_of(entity: u64) -> f32 {
    (entity % 97) as f32 * 0.5
}

/// Where the fixture places entity `e`: spread over the whole extent.
fn spread(e: u64) -> (f64, f64) {
    (((e * 37) % 1000) as f64, ((e * 53) % 1000) as f64)
}

/// The fixture's points file, entity `e` at `at(e)`, plus the three attribute columns keyed to
/// `entity_id`.
///
/// The category arrives as its **key**, never as a code (§3.1): the code is assigned once, in the
/// schema, and a data file supplying codes directly would be a second place codes are decided.
fn write_points_with_attributes(path: &Path, n: u64, at: impl Fn(u64) -> (f64, f64)) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("band", DataType::Utf8, false),
        Field::new("ingested_at", DataType::Int64, false),
        Field::new("score", DataType::Float32, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let xs: Vec<f64> = ids.iter().map(|&e| at(e).0).collect();
    let ys: Vec<f64> = ids.iter().map(|&e| at(e).1).collect();
    let bands: Vec<&str> = ids.iter().map(|e| band_of(*e)).collect();
    let stamps: Vec<i64> = ids.iter().map(|e| ingested_at_of(*e)).collect();
    let scores: Vec<f32> = ids.iter().map(|e| score_of(*e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(bands)),
            Arc::new(arrow::array::Int64Array::from(stamps)),
            Arc::new(arrow::array::Float32Array::from(scores)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn parse_schema(tmp: &Path) -> Schema {
    let path = tmp.join("schema.toml");
    std::fs::write(&path, SCHEMA_TOML).unwrap();
    Config::parse(&path, &std::collections::HashMap::new())
        .map(|c| c.schema)
        .expect("the fixture schema parses")
}

/// Build a fixture bundle carrying the attribute tail.
fn build_fixture_with_attributes(out: &Path, tmp: &Path, n: u64) {
    build_placed_fixture(out, tmp, n, spread);
}

/// [`build_fixture_with_attributes`] with entity `e` at `at(e)`.
fn build_placed_fixture(out: &Path, tmp: &Path, n: u64, at: impl Fn(u64) -> (f64, f64)) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points_with_attributes(&points, n, at);
    write_pairs_n(&pairs, n);
    let schema = parse_schema(tmp);
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points.clone(),
            &with_id(schema.clone()),
        ),
        out: out.to_path_buf(),
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
        schema: with_id(schema),
    };
    build(&args).expect("a build with a declared schema should succeed");
}

/// Every `(mosaica_id, band, ingested_at, score)` in every live segment of the bundle at `root`.
///
/// **Read through `ColumnsRef` by name**, which is how the serving path reads it — so a segment
/// whose tail is present but misnamed, mistyped or short fails here exactly as it would in a
/// request. Keyed by `mosaica_id` because every producer may reorder rows (see the module doc).
fn tail_by_identity(root: &Path) -> BTreeMap<u64, (u8, i64, f32)> {
    let bundle = open_bundle(root).expect("the bundle opens");
    // The prefix is read from `CURRENT` rather than assumed, because a fold publishes into a new
    // one — a hard-coded `v00000` would silently read the *pre-fold* segments and let the fold
    // case pass without the fold's output ever being looked at.
    let current: mosaica_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT is readable"))
            .expect("CURRENT parses");
    let prefix = &current.prefix;
    let mut out = BTreeMap::new();
    for (phash, partition) in &bundle.partitions {
        for segment in &partition.manifest.segments {
            let dir = root
                .join(prefix)
                .join("partitions")
                .join(phash)
                .join("views")
                .join(&segment.view)
                .join("segments")
                .join(&segment.seg_id);
            let columns = ColumnsRef::load(&dir.join("columns.arrow"))
                .unwrap_or_else(|e| panic!("segment {} must open: {e}", segment.seg_id));
            let ids = columns.mosaica_id();
            let band = match columns.scalar("band") {
                Some(ScalarSlice::U8(v)) => v,
                other => panic!(
                    "segment {} must carry 'band' as u8, found {other:?}",
                    segment.seg_id
                ),
            };
            let stamp = match columns.scalar("ingested_at") {
                Some(ScalarSlice::I64(v)) => v,
                other => panic!(
                    "segment {} must carry 'ingested_at' as i64, found {other:?}",
                    segment.seg_id
                ),
            };
            let score = match columns.scalar("score") {
                Some(ScalarSlice::F32(v)) => v,
                other => panic!(
                    "segment {} must carry 'score' as f32, found {other:?}",
                    segment.seg_id
                ),
            };
            assert_eq!(ids.len(), band.len(), "every column is one per row");
            assert_eq!(ids.len(), stamp.len());
            assert_eq!(ids.len(), score.len());
            for row in 0..ids.len() {
                out.insert(ids[row], (band[row], stamp[row], score[row]));
            }
        }
    }
    out
}

fn engine_over(tmp: &Path, root: &Path, config: EngineConfig) -> Engine {
    let mut engine = Engine::open(root, &tmp.join("cache"), &tmp.join("wal.log"), config)
        .expect("the engine opens against a bundle carrying a declared tail");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

// ---------------------------------------------------------------------------------------------

/// **The build writes the tail, and the manifest describes it.**
///
/// The floor everything else stands on: before this, `declared_scalars` was written empty
/// unconditionally and no build path emitted a column, so the whole tail — the ingest validation,
/// the flush schema, the reader's widening — had never run against a non-empty declaration.
#[test]
fn a_build_emits_the_declared_tail_and_records_its_vocabulary() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_attributes(&root, tmp.path(), N_ITEMS);

    let bundle = open_bundle(&root).expect("the bundle opens");
    let declared = &bundle.manifest.declared_scalars;
    assert_eq!(
        declared.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        vec!["band", "ingested_at", "score", "id"],
        "declaration order is the column order and must survive compilation verbatim"
    );
    assert_eq!(
        declared
            .iter()
            .map(|d| d.arrow_type.arrow_type_name())
            .collect::<Vec<_>>(),
        vec!["u8", "i64", "f32", "u64"]
    );
    // The category names its vocabulary; the two plain scalars name none.
    assert_eq!(declared[0].vocabulary.as_deref(), Some("band"));
    assert!(declared[1].vocabulary.is_none());
    assert!(declared[2].vocabulary.is_none());

    let vocabulary = bundle
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "band")
        .expect("the declared vocabulary reaches the manifest");
    assert_eq!(
        vocabulary.visibility,
        mosaica_store::manifest::Visibility::Public
    );
    let codes: BTreeMap<&str, u32> = vocabulary
        .values
        .iter()
        .map(|v| (v.key.as_str(), v.code))
        .collect();
    assert_eq!(codes, BTreeMap::from([("low", 1), ("mid", 2), ("high", 3)]));

    // And the values are at the rows, joined **source id → entity → mosaica_id**.
    //
    // **The source id is not the entity id, and asserting as if it were is how this case passed
    // against a build that gave every item another item's attributes.** Entity ids are assigned
    // in signature-sorted order (§11.1), so the map is a permutation with no fixed points to
    // speak of — but a fixture whose items all carry one signature has an *identity* permutation,
    // which is what let the wrong assertion look right. It is read from the index of the unique
    // `id` the build joins on, which is the bundle's own record of the assignment rather than a second guess at it.
    let tail = tail_by_identity(&root);
    assert_eq!(tail.len(), N_ITEMS as usize);
    let entity_of_source = source_to_new_map(&root, "v00000");
    assert_eq!(entity_of_source.len(), N_ITEMS as usize);
    let key = test_key();
    for source in 0..N_ITEMS {
        let entity = entity_of_source[&source];
        let id = key
            .forward(
                ItemHigh::new(0, Tenancy::ZERO),
                mosaica_types::EntityId::new(entity),
            )
            .unwrap();
        let (band, stamp, score) = tail[&id.raw()];
        assert_eq!(band, band_code(source), "source {source}'s band code");
        assert_eq!(stamp, ingested_at_of(source), "source {source}'s timestamp");
        assert_eq!(score, score_of(source), "source {source}'s score");
    }
}

/// **The two build implementations agree about the tail, byte for byte.**
///
/// `build_in_memory` is the oracle the streaming pipeline is tested against precisely because the
/// entity assignment it encodes is permanent (I9). The pipeline resolves a source id to an entity
/// through `source_ids` and `entity_of_ordinal`; the in-memory build resolves it through the
/// staged items' own order. Those are two independent implementations of one mapping, and this is
/// what stops them drifting — the defect the wrong version of the case above could not see.
#[test]
fn both_build_implementations_write_the_same_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    write_points_with_attributes(&points, 2_000, spread);
    write_pairs_n(&pairs, 2_000);
    let schema = parse_schema(tmp.path());
    let args_for = |out: &Path| BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points.clone(),
            &with_id(schema.clone()),
        ),
        out: out.to_path_buf(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: with_id(schema.clone()),
    };

    let streamed = tmp.path().join("streamed");
    let linear = tmp.path().join("linear");
    build(&args_for(&streamed)).expect("the streaming build succeeds");
    mosaica_build::build_in_memory(&args_for(&linear)).expect("the in-memory build succeeds");

    let a = std::fs::read(
        streamed.join("v00000/partitions/default/views/s0/segments/seg-0/columns.arrow"),
    )
    .unwrap();
    let b = std::fs::read(
        linear.join("v00000/partitions/default/views/s0/segments/seg-0/columns.arrow"),
    )
    .unwrap();
    assert_eq!(
        a, b,
        "the two builds' columns.arrow must be byte-identical, tail included"
    );
}

/// **A flush writes the same tail the build did**, taking its schema from the manifest.
///
/// ⊘ **Ingest carries codes, not keys.** §5's declare-then-use rule is about a *key* arriving at
/// ingest and being refused if the vocabulary does not declare it; that mapping does not exist —
/// the batch's scalar tail is validated against the declared *arrow type*, so a `u8` category
/// column carries the code. What this case pins is the placement, not the key mapping.
#[test]
fn an_ingested_row_carries_the_declared_tail_through_a_flush() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_attributes(&root, tmp.path(), N_ITEMS);
    let engine = engine_over(tmp.path(), &root, config());

    let entity = engine
        .ingest_rows(
            vec![UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                descriptors: vec![b"0".to_vec()],
                x: 5.0,
                y: 5.0,
                // Positional, in the manifest's declared order — the order the whole path reads
                // it back in.
                scalars: vec![
                    WalScalar::U8(3),
                    WalScalar::I64(1_800_000_000_000_000),
                    WalScalar::F32(12.5),
                ],
                terms: engine.resolve_terms(&[b"0".to_vec()]),
                scoped: Vec::new(),
            }],
            "batch-1".to_string(),
            [0u8; 32],
        )
        .expect("an ingest carrying the declared tail is accepted")[0];

    publish_buffered(&engine);
    drop(engine);

    let tail = tail_by_identity(&root);
    assert_eq!(
        tail.len(),
        N_ITEMS as usize + 1,
        "the flushed row joins the base segment's rows"
    );
    let id = test_key()
        .forward(ItemHigh::new(0, Tenancy::ZERO), entity)
        .unwrap();
    assert_eq!(
        tail[&id.raw()],
        (3u8, 1_800_000_000_000_000i64, 12.5f32),
        "the flushed row's tail is what was ingested"
    );
}

/// **A merge carries every input segment's tail forward, permuted with its rows.**
///
/// A merge is the producer with the most opportunity to lose the tail quietly: it reads *k* mapped
/// segments and interleaves them, so a value carried forward at the wrong index lands on a real
/// row of a real item. Several flushes are merged here rather than one, because a single-input
/// merge cannot interleave and so cannot show the defect.
#[test]
fn a_merge_carries_every_inputs_tail_forward_against_the_right_identities() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_attributes(&root, tmp.path(), N_ITEMS);
    // A merge cap low enough that the flushes below select one, and a base segment far above it.
    let engine = engine_over(
        tmp.path(),
        &root,
        EngineConfig {
            max_merged_segment_bytes: Some(64 * 1024),
            ..config()
        },
    );

    let mut expected = BTreeMap::new();
    for batch in 0..6u64 {
        let entity = engine
            .ingest_rows(
                vec![UnallocatedRow {
                    view: "s0".to_string(),
                    join: None,
                    descriptors: vec![b"0".to_vec()],
                    // Spread across the extent so the merge genuinely interleaves in Morton order
                    // rather than appending one segment after another.
                    x: (batch * 149 % 1000) as f64,
                    y: (batch * 271 % 1000) as f64,
                    scalars: vec![
                        WalScalar::U8((batch % 3) as u8 + 1),
                        WalScalar::I64(1_900_000_000_000_000 + batch as i64),
                        WalScalar::F32(batch as f32 * 3.25),
                    ],
                    terms: engine.resolve_terms(&[b"0".to_vec()]),
                    scoped: Vec::new(),
                }],
                format!("batch-{batch}"),
                [batch as u8; 32],
            )
            .expect("accepted")[0];
        publish_buffered(&engine);
        let id = test_key()
            .forward(ItemHigh::new(0, Tenancy::ZERO), entity)
            .unwrap();
        expected.insert(
            id.raw(),
            (
                (batch % 3) as u8 + 1,
                1_900_000_000_000_000 + batch as i64,
                batch as f32 * 3.25,
            ),
        );
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().merges == 0 {
        assert!(std::time::Instant::now() < deadline, "no merge ran");
        engine.request_flush();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(engine);

    let tail = tail_by_identity(&root);
    for (id, want) in &expected {
        assert_eq!(
            tail.get(id),
            Some(want),
            "a merged row's tail must follow its identity, not its old row index"
        );
    }
    // And the base segment's rows are untouched by a merge that did not include it — joined
    // through the sidecar, for the reason the build case states.
    let entity_of_source = source_to_new_map(&root, "v00000");
    let key = test_key();
    for source in [0u64, 1, 2, N_ITEMS - 1] {
        let entity = entity_of_source[&source];
        let id = key
            .forward(
                ItemHigh::new(0, Tenancy::ZERO),
                mosaica_types::EntityId::new(entity),
            )
            .unwrap();
        assert_eq!(
            tail[&id.raw()],
            (band_code(source), ingested_at_of(source), score_of(source))
        );
    }
}

/// **The fold rewrites every row of the corpus and must rewrite every column of it.**
///
/// Compaction's pass 1 re-emits the whole segment into a new prefix under a new permutation, so
/// this is the one producer that touches every row the build wrote. A fold that took its writer
/// schema from anywhere but the manifest would publish a base segment shorter than the manifest
/// declares — and `compact::execute` refuses to start when `scalar_schema_of` returns `None`,
/// which is the fail-closed half this case's sibling covers.
#[test]
fn a_fold_rewrites_the_whole_corpus_without_losing_the_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_attributes(&root, tmp.path(), N_ITEMS);
    let engine = engine_over(tmp.path(), &root, config());

    let before = tail_by_identity(&root);
    assert_eq!(before.len(), N_ITEMS as usize);

    // An ingest and a flush first, so the fold has a delta to fold in as well as a base to rewrite
    // — a fold over the base alone would not exercise the k-way path the tail travels through.
    let ingested = engine
        .ingest_rows(
            vec![UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                descriptors: vec![b"0".to_vec()],
                x: 500.0,
                y: 500.0,
                scalars: vec![
                    WalScalar::U8(2),
                    WalScalar::I64(2_000_000_000_000_000),
                    WalScalar::F32(99.75),
                ],
                terms: engine.resolve_terms(&[b"0".to_vec()]),
                scoped: Vec::new(),
            }],
            "pre-fold".to_string(),
            [7u8; 32],
        )
        .expect("accepted")[0];
    publish_buffered(&engine);
    fold(&engine);
    drop(engine);

    let after = tail_by_identity(&root);
    assert_eq!(
        after.len(),
        N_ITEMS as usize + 1,
        "a fold reclaims nothing here, so every row must survive it"
    );
    for (id, want) in &before {
        assert_eq!(
            after.get(id),
            Some(want),
            "identity {id}'s tail changed across a fold that folded nothing away"
        );
    }
    let id = test_key()
        .forward(ItemHigh::new(0, Tenancy::ZERO), ingested)
        .unwrap();
    assert_eq!(
        after[&id.raw()],
        (2u8, 2_000_000_000_000_000i64, 99.75f32),
        "the flushed row's tail survives being folded into the new base"
    );

    // **And the vocabulary survives the fold.** A code is meaningless without its binding: a fold
    // that rewrote every row correctly and dropped `MANIFEST.vocabularies` would leave a corpus
    // whose marks all decode to nothing, and nothing in the row data would be wrong. The fold
    // writes its manifest by cloning the live one and amending two fields, so bindings are
    // carried forward rather than re-derived — this is what stops that becoming a restatement
    // somebody has to keep complete.
    let folded = open_bundle(&root).expect("the folded bundle opens");
    let vocabulary = folded
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "band")
        .expect("the vocabulary survives the fold");
    assert_eq!(
        vocabulary.values.len(),
        3,
        "every binding survives, not merely the ones a surviving row happens to use"
    );
    assert_eq!(
        folded
            .manifest
            .declared_scalars
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        vec!["band", "ingested_at", "score", "id"],
        "the tail's declared order survives the fold — it is what every reader reads by position"
    );
}

/// **The read path binds a served point's scalars to that point's identity.**
///
/// Everything above this test asserts the four *producers* of `columns.arrow` — it reads the file
/// off disk and never calls `Engine::viewport`. That left the gather itself, which is what turns
/// stored columns into a response, covered by nothing: no Rust test in the workspace read a served
/// point's scalars at all, and the TypeScript decoder test asserts only that each column has the
/// right length and type. A gather that returned every value permuted, or that paired column *i*'s
/// values with column *j*'s name, passed the entire suite.
///
/// That gap is why this exists, and it is why the assertion is **per identity**: `tail_by_identity`
/// gives the truth from the segment files, and this checks the served tail against it point by
/// point. A row-indexed assertion would pass on a gather that carried values forward unpermuted.
///
/// **Two segments and a multi-tile viewport**, because the interesting failures need both. The
/// flush gives the view a second segment, so a tile's rows resolve to different parts and any
/// per-part hoisting has to key correctly; `zoom = 3` spans many tiles, so the per-tile results
/// have to concatenate in tile order. The schema's three widths are what make a positional slip a
/// type error rather than a plausible value (see this file's header).
#[test]
fn a_served_point_carries_its_own_tail_across_segments_and_tiles() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_attributes(&root, tmp.path(), N_ITEMS);
    let engine = engine_over(tmp.path(), &root, config_uncapped());

    let ingested = engine
        .ingest_rows(
            vec![UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                descriptors: vec![b"0".to_vec()],
                x: 5.0,
                y: 5.0,
                scalars: vec![
                    WalScalar::U8(2),
                    WalScalar::I64(1_900_000_000_000_000),
                    WalScalar::F32(99.5),
                ],
                terms: engine.resolve_terms(&[b"0".to_vec()]),
                scoped: Vec::new(),
            }],
            "batch-read-path".to_string(),
            [0u8; 32],
        )
        .expect("the ingest is accepted")[0];
    publish_buffered(&engine);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 3, [0.0, 0.0, 1000.0, 1000.0], u32::MAX as usize),
        )
        .expect("a viewport over both segments");

    assert!(
        out.tiles.len() > 1,
        "the point of this case is a MULTI-tile response: {} tile(s)",
        out.tiles.len()
    );
    assert_eq!(
        out.scalar_names,
        vec!["band", "ingested_at", "score"],
        "the names are the render declaration's, in its order"
    );
    assert_eq!(
        out.points.scalars.len(),
        3,
        "one buffer per render column, whatever any tile happened to hold"
    );
    for (i, column) in out.points.scalars.iter().enumerate() {
        assert_eq!(
            column.len(),
            out.points.len(),
            "column {i} is short — the tile concatenation dropped values"
        );
    }

    let truth = tail_by_identity(&root);
    let (band, ingested_at, score) = match (
        &out.points.scalars[0].values,
        &out.points.scalars[1].values,
        &out.points.scalars[2].values,
    ) {
        (ColumnBuf::U8(b), ColumnBuf::I64(t), ColumnBuf::F32(s)) => (b, t, s),
        other => panic!("the tail came back at the wrong types: {other:?}"),
    };

    assert!(!out.points.is_empty(), "the viewport served nothing");
    for (i, (mosaica_id, _code)) in out.points.iter().enumerate() {
        let expected = truth
            .get(&mosaica_id.raw())
            .unwrap_or_else(|| panic!("served a point ({mosaica_id:?}) the segments do not hold"));
        assert_eq!(
            (band[i], ingested_at[i], score[i]),
            *expected,
            "point {i} ({mosaica_id:?}) carries another point's tail"
        );
    }

    // The flushed row is in a different segment from every other point, so its presence is what
    // proves the per-part resolution is keyed rather than assumed.
    let flushed = test_key()
        .forward(ItemHigh::new(0, Tenancy::ZERO), ingested)
        .unwrap();
    let position = out
        .points
        .iter()
        .position(|(id, _)| id == flushed)
        .expect("the flushed item is served, not merely counted");
    assert_eq!(
        (band[position], ingested_at[position], score[position]),
        (2u8, 1_900_000_000_000_000i64, 99.5f32),
        "the flushed row's tail is what was ingested"
    );
}

/// **Every served point's id, position and values come from the segment that holds it**, over a
/// view of three segments (the build and two flushes, merging off) and tiles that each span a
/// different set of them. Zoom 2 over the 1000-unit extent gives 250-unit tiles, served in raster
/// order:
///
/// - `(0, 0)` holds build rows only, and is served first;
/// - `(1, 0)` holds build rows and the second flush's, some of which have no score, so the nulls
///   are in a part that is not the tile's first;
/// - `(2, 0)` holds the first flush's rows only;
/// - `(3, 3)` holds rows of all three segments.
///
/// The second flush's rows in `(1, 0)` climb a diagonal, which fixes their order in the segment:
/// its rows 4 to 11 carry a score and the rest do not, and the build's rows in the same tile are
/// its rows 4 to 7. Each point is checked against the segment files as stored.
#[test]
fn every_point_reads_the_segment_that_holds_it_across_tiles_of_several_segments() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    // Four build rows in each of `(0, 0)`, `(1, 0)` and `(3, 3)`.
    build_placed_fixture(&root, tmp.path(), 12, |e| {
        let j = (e % 4) as f64;
        match e / 4 {
            0 => (20.0 + 30.0 * j, 20.0 + 30.0 * j),
            1 => (310.0 + 40.0 * j, 230.0),
            _ => (800.0 + 30.0 * j, 800.0 + 30.0 * j),
        }
    });
    let engine = engine_over(tmp.path(), &root, config_uncapped());
    engine.set_merge_for_test(false);

    let ingest = |batch: &str, rows: Vec<((f64, f64), Option<f32>)>| {
        let rows = rows
            .into_iter()
            .enumerate()
            .map(|(i, ((x, y), score))| UnallocatedRow {
                view: "s0".to_string(),
                join: None,
                descriptors: vec![b"0".to_vec()],
                x,
                y,
                scalars: vec![
                    WalScalar::U8(2),
                    WalScalar::I64(1_900_000_000_000_000 + i as i64),
                    score.map_or(WalScalar::Null, WalScalar::F32),
                ],
                terms: engine.resolve_terms(&[b"0".to_vec()]),
                scoped: Vec::new(),
            })
            .collect();
        engine
            .ingest_rows(rows, batch.to_string(), [0u8; 32])
            .expect("the ingest is accepted");
        publish_buffered(&engine);
    };
    // Five rows in `(2, 0)` and two in `(3, 3)`, every one scored.
    ingest(
        "first",
        (0..5)
            .map(|i| (520.0 + 40.0 * i as f64, 30.0 + 40.0 * i as f64))
            .chain((0..2).map(|i| (815.0 + 40.0 * i as f64, 905.0)))
            .enumerate()
            .map(|(i, at)| (at, Some(10.0 + i as f32)))
            .collect(),
    );
    // Twenty rows up a diagonal of `(1, 0)`, then three in `(3, 3)` of which the first is scored.
    ingest(
        "second",
        (0..20)
            .map(|i| {
                let at = (255.0 + 10.0 * i as f64, 5.0 + 10.0 * i as f64);
                (at, (4..12).contains(&i).then_some(20.0 + i as f32))
            })
            .chain((0..3).map(|i| ((905.0 + 30.0 * i as f64, 955.0), (i == 0).then_some(40.0))))
            .collect(),
    );

    // The stored truth, keyed by `mosaica_id`: which segment holds the row, its position code and
    // its three values, with the score read through the segment's presence record.
    struct Stored {
        segment: usize,
        code: u64,
        band: u8,
        ingested_at: i64,
        score: Option<f32>,
    }
    let bundle = open_bundle(&root).expect("the bundle opens");
    let view_data = bundle
        .partitions
        .values()
        .find_map(|p| p.views.get("s0"))
        .expect("the bundle holds view s0");
    let segments = mosaica_engine::viewport::segments_with_row_bases("s0", view_data).unwrap();
    assert_eq!(segments.len(), 3, "the build and two flushes, unmerged");
    let mut stored = BTreeMap::new();
    for (at, (segment, _)) in segments.iter().enumerate() {
        let columns = &segment.columns;
        let other = || panic!("segment {} holds the tail at other types", segment.seg_id);
        let Some(ScalarSlice::U8(band)) = columns.scalar("band") else {
            other()
        };
        let Some(ScalarSlice::I64(stamp)) = columns.scalar("ingested_at") else {
            other()
        };
        let Some(ScalarSlice::F32(score)) = columns.scalar("score") else {
            other()
        };
        let presence = columns.presence("score");
        for row in 0..columns.row_count() as usize {
            let high = (segment.morton.u32()[row] as u64) << 32;
            stored.insert(
                columns.mosaica_id()[row],
                Stored {
                    segment: at,
                    code: high | columns.residual()[row] as u64,
                    band: band[row],
                    ingested_at: stamp[row],
                    score: presence.contains(row as u32).then_some(score[row]),
                },
            );
        }
    }
    assert_eq!(stored.len(), 42);
    assert_eq!(stored.values().filter(|s| s.score.is_none()).count(), 14);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 2, [0.0, 0.0, 999.0, 999.0], u32::MAX as usize),
        )
        .expect("a viewport over the three segments");
    // Prefixes at zoom 2, x in the even bits: `(0, 0)` 0, `(1, 0)` 1, `(2, 0)` 4, `(3, 3)` 15.
    assert_eq!(
        out.tiles.iter().map(|t| t.tile).collect::<Vec<_>>(),
        vec![0, 1, 4, 15]
    );
    let (ColumnBuf::U8(band), ColumnBuf::I64(ingested_at), ColumnBuf::F32(score)) = (
        &out.points.scalars[0].values,
        &out.points.scalars[1].values,
        &out.points.scalars[2].values,
    ) else {
        panic!("the tail came back at other types");
    };
    let score_column = &out.points.scalars[2];

    let mut seen = BTreeSet::new();
    let mut segments_of_tile = BTreeMap::new();
    let mut i = 0;
    for tile in &out.tiles {
        let mut held = BTreeSet::new();
        for _ in 0..tile.served {
            let (id, code) = (out.points.mosaica_ids[i], out.points.codes[i]);
            let truth = stored
                .get(&id)
                .unwrap_or_else(|| panic!("point {i} has an id no segment holds"));
            assert!(seen.insert(id), "point {i} is served twice");
            assert_eq!(code, truth.code, "point {i}'s position");
            assert_eq!(
                code >> 60,
                tile.tile,
                "point {i} lies in the tile it is served under"
            );
            assert_eq!(band[i], truth.band, "point {i}'s band");
            assert_eq!(ingested_at[i], truth.ingested_at, "point {i}'s ingested_at");
            assert_eq!(
                score_column.is_present(i).then_some(score[i]),
                truth.score,
                "point {i}'s score"
            );
            held.insert((truth.segment, truth.score.is_none()));
            i += 1;
        }
        segments_of_tile.insert(tile.tile, held);
    }
    assert_eq!(i, out.points.len());
    assert_eq!(seen.len(), stored.len(), "every stored row is served");

    let segments_in = |tile: u64| -> BTreeSet<usize> {
        segments_of_tile[&tile].iter().map(|&(s, _)| s).collect()
    };
    assert_eq!(segments_in(0), BTreeSet::from([0]));
    assert_eq!(segments_in(1), BTreeSet::from([0, 2]));
    assert!(
        segments_of_tile[&1].contains(&(2, true)),
        "(1, 0) serves an unscored flushed row"
    );
    assert_eq!(segments_in(4), BTreeSet::from([1]));
    assert_eq!(segments_in(15), BTreeSet::from([0, 1, 2]));
}

/// **A segment holding a render column at a type other than the declared one is refused.** The
/// manifest is rewritten after the build to declare `score` as `f64` while the segment stores
/// `f32`, and the digest in `CURRENT` follows it, so the bundle opens and the disagreement
/// reaches the gather.
#[test]
fn a_segment_holding_a_render_column_at_another_type_is_refused() {
    use sha2::{Digest, Sha256};

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_attributes(&root, tmp.path(), N_ITEMS);

    let current_path = root.join("CURRENT");
    let mut current: mosaica_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(&current_path).unwrap()).unwrap();
    let manifest_path = root.join(&current.prefix).join("MANIFEST.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    let score = manifest["declared_scalars"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|d| d["name"] == "score")
        .expect("the fixture declares score");
    assert_eq!(score["arrow_type"], "f32");
    score["arrow_type"] = "f64".into();
    let bytes = serde_json::to_vec(&manifest).unwrap();
    std::fs::write(&manifest_path, &bytes).unwrap();
    current.manifest_digest = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(&current_path, serde_json::to_vec(&current).unwrap()).unwrap();

    let engine = engine_over(tmp.path(), &root, config_uncapped());
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let answer = engine.viewport(
        &session,
        ViewportRequest::new("s0", 3, [0.0, 0.0, 1000.0, 1000.0], u32::MAX as usize),
    );
    assert!(
        matches!(answer, Err(mosaica_engine::EngineError::Malformed(_))),
        "a segment storing score as f32 under an f64 declaration is refused: {:?}",
        answer.map(|out| out.points.len())
    );
}

// =============================================================================================
// A render set that is not a prefix of the declaration
// =============================================================================================

/// The non-prefix fixture's schema: an **entity-space** `i64` declared first, then the rendered
/// `u8` category and `f32`. The render columns sit at declared positions 1 and 2, so no render
/// column shares a position with its place in the full declaration — the one shape that exposes
/// a consumer pairing render buffers with full-declaration names, which every-column-rendered
/// fixtures cannot. The widths differ for the file-header reason: a positional slip is a type
/// mismatch, never a plausible value.
const NON_PREFIX_SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "band"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  low = 1
  mid = 2
  high = 3

[[attribute]]
name  = "audit"
type  = "i64"
index = true

[[attribute]]
name       = "band"
type       = "category"
render     = true
vocabulary = "band"

[[attribute]]
name   = "score"
type   = "f32"
render = true
"#;

fn audit_of(source: u64) -> i64 {
    9_000_000 + source as i64 * 7
}

fn write_points_non_prefix(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("audit", DataType::Int64, false),
        Field::new("band", DataType::Utf8, false),
        Field::new("score", DataType::Float32, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 37) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 53) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(arrow::array::Int64Array::from(
                ids.iter().map(|e| audit_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| band_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(arrow::array::Float32Array::from(
                ids.iter().map(|e| score_of(*e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_non_prefix_fixture(out: &Path, tmp: &Path, n: u64) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points_non_prefix(&points, n);
    write_pairs_n(&pairs, n);
    let schema_path = tmp.join("schema.toml");
    std::fs::write(&schema_path, NON_PREFIX_SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &std::collections::HashMap::new())
        .map(|c| c.schema)
        .expect("the non-prefix fixture schema parses");
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points.clone(),
            &with_id(schema.clone()),
        ),
        out: out.to_path_buf(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: with_id(schema),
    };
    build(&args).expect("a build whose render set is not a declaration prefix succeeds");
}

/// One ingest row for the non-prefix fixture — scalars positional over the **full** declaration
/// (the ingest plane's shape); the flush narrows to the render columns before it writes.
fn non_prefix_row(engine: &Engine, audit: i64, band_code: u8, score: f32) -> UnallocatedRow {
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: vec![
            WalScalar::I64(audit),
            WalScalar::U8(band_code),
            WalScalar::F32(score),
        ],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    }
}

/// Every `(mosaica_id, band, score)` in every live segment — the render tail read by name, the
/// same truth-by-identity join `tail_by_identity` performs for the all-rendered fixture.
fn non_prefix_tail_by_identity(root: &Path) -> BTreeMap<u64, (u8, f32)> {
    let bundle = open_bundle(root).expect("the bundle opens");
    let current: mosaica_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT is readable"))
            .expect("CURRENT parses");
    let prefix = &current.prefix;
    let mut out = BTreeMap::new();
    for (phash, partition) in &bundle.partitions {
        for segment in &partition.manifest.segments {
            let dir = root
                .join(prefix)
                .join("partitions")
                .join(phash)
                .join("views")
                .join(&segment.view)
                .join("segments")
                .join(&segment.seg_id);
            let columns = ColumnsRef::load(&dir.join("columns.arrow"))
                .unwrap_or_else(|e| panic!("segment {} must open: {e}", segment.seg_id));
            let ids = columns.mosaica_id();
            let band = match columns.scalar("band") {
                Some(ScalarSlice::U8(v)) => v,
                other => panic!(
                    "segment {} must carry 'band' as u8, found {other:?}",
                    segment.seg_id
                ),
            };
            let score = match columns.scalar("score") {
                Some(ScalarSlice::F32(v)) => v,
                other => panic!(
                    "segment {} must carry 'score' as f32, found {other:?}",
                    segment.seg_id
                ),
            };
            for row in 0..ids.len() {
                out.insert(ids[row], (band[row], score[row]));
            }
        }
    }
    out
}

/// **Every served points column arrives under its own name when the render set is not a prefix
/// of the declaration.** The head's schema and the gathered buffers are both the render
/// narrowing; a head carrying the full declaration instead served `band`'s codes under `audit`'s
/// name and `score`'s values under `band`'s — silently, since a client reads by name, and
/// invisibly to every fixture whose first columns render. Both segments participate: the
/// build's and a flushed one, so the flush's own positional narrowing is under the same check.
#[test]
fn a_non_prefix_render_declaration_serves_every_column_under_its_own_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_non_prefix_fixture(&root, tmp.path(), 512);
    let engine = engine_over(tmp.path(), &root, config_uncapped());

    let flushed_entity = engine
        .ingest_rows(
            vec![non_prefix_row(&engine, 4242, 2, 9.25)],
            "batch-non-prefix".to_string(),
            [0u8; 32],
        )
        .expect("the ingest is accepted")[0];
    publish_buffered(&engine);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let out = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 3, [0.0, 0.0, 1000.0, 1000.0], u32::MAX as usize),
        )
        .expect("a viewport over both segments");

    assert_eq!(
        out.scalar_names,
        vec!["band", "score"],
        "the render columns' names in declaration order — `audit` is entity-space and absent"
    );
    assert_eq!(
        out.points.scalars.len(),
        out.scalar_names.len(),
        "one buffer per name: the two lists zip positionally on the wire"
    );
    assert_eq!(
        out.points.len(),
        513,
        "every row of both segments is served"
    );
    let flushed_id = engine.mosaica_id_of(flushed_entity).unwrap();
    assert!(
        out.points.iter().any(|(id, _)| id == flushed_id),
        "the flushed item is served, not merely counted"
    );

    // Read by name, exactly as a client does; truth joined by identity from the segments.
    let truth = non_prefix_tail_by_identity(&root);
    let position = |name: &str| {
        out.scalar_names
            .iter()
            .position(|n| n.as_str() == name)
            .unwrap()
    };
    let band = match &out.points.scalars[position("band")].values {
        ColumnBuf::U8(v) => v,
        other => panic!("the column named 'band' must be u8, found {other:?}"),
    };
    let score = match &out.points.scalars[position("score")].values {
        ColumnBuf::F32(v) => v,
        other => panic!("the column named 'score' must be f32, found {other:?}"),
    };
    for (row, (id, _)) in out.points.iter().enumerate() {
        let (expected_band, expected_score) = truth[&id.raw()];
        assert_eq!(
            band[row], expected_band,
            "row {row}'s band, under its own name"
        );
        assert_eq!(
            score[row], expected_score,
            "row {row}'s score, under its own name"
        );
    }

    // A counts-only request seeds its empty columns from the same render schema the gather
    // fills, so the two shapes cannot disagree about the column set.
    let counts_only = engine
        .viewport(
            &session,
            ViewportRequest::new("s0", 3, [0.0, 0.0, 1000.0, 1000.0], 0),
        )
        .expect("a counts-only viewport");
    assert!(counts_only.points.is_empty());
    assert_eq!(counts_only.scalar_names, vec!["band", "score"]);
    assert_eq!(counts_only.points.scalars.len(), 2);
    assert!(
        matches!(counts_only.points.scalars[0].values, ColumnBuf::U8(_)),
        "the seeded empty column carries the render column's type, not the declaration's first"
    );
    assert!(matches!(
        counts_only.points.scalars[1].values,
        ColumnBuf::F32(_)
    ));
}

/// **Drill-down under the same non-prefix declaration: every home's value under its own name.**
/// The record is assembled per declared column — the render tail selected positionally through
/// `Manifest::render_indices`, the entity-space column by name — and served as name/value pairs,
/// so this pins that no home's value shifts under a neighbouring column's name when the render
/// list and the declaration diverge. Checked for built rows and a flushed one.
#[test]
fn a_drill_down_assembles_the_non_prefix_declaration_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_non_prefix_fixture(&root, tmp.path(), 64);
    let engine = engine_over(tmp.path(), &root, config_uncapped());

    let flushed_entity = engine
        .ingest_rows(
            vec![non_prefix_row(&engine, 4242, 2, 9.25)],
            "batch-non-prefix-drill".to_string(),
            [0u8; 32],
        )
        .expect("the ingest is accepted")[0];
    publish_buffered(&engine);

    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let expect_item = |id, audit: i64, band_key: &str, score: f32, label: &str| {
        let served = engine
            .item(&session, id)
            .expect("drill-down succeeds")
            .unwrap_or_else(|| panic!("{label} is visible to full coverage"));
        // `id` only finds the built items; the columns under test are the others.
        let names: Vec<&str> = served
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .filter(|name| *name != "id")
            .collect();
        assert_eq!(
            names,
            ["audit", "band", "score"],
            "{label}: every declared column has a value in some home, in declared order"
        );
        let value = |name: &str| &served.fields.iter().find(|f| f.name == name).unwrap().value;
        assert_eq!(
            value("audit"),
            &mosaica_engine::ScalarOut::I64(audit),
            "{label}: the entity-space value under its own name"
        );
        assert_eq!(
            value("band"),
            &mosaica_engine::ScalarOut::Utf8(band_key.to_string()),
            "{label}: the category resolves to its own key"
        );
        assert_eq!(
            value("score"),
            &mosaica_engine::ScalarOut::F32(score),
            "{label}: the row value under its own name"
        );
    };

    let entity_of_source = source_to_new_map(&root, "v00000");
    for source in [0u64, 1, 5, 63] {
        let id = engine
            .mosaica_id_of(mosaica_types::EntityId::new(entity_of_source[&source]))
            .expect("identity is computable");
        expect_item(
            id,
            audit_of(source),
            band_of(source),
            score_of(source),
            &format!("built source {source}"),
        );
    }
    let flushed_id = engine.mosaica_id_of(flushed_entity).unwrap();
    expect_item(flushed_id, 4242, "mid", 9.25, "the flushed item");
}

// =============================================================================================
// The third home: the record blob through flush, coalesce and fold (records §3, §7)
// =============================================================================================

/// The record fixture's schema: a rendered `u8` category plus two **blob-resident** columns — a
/// `keyword` note and an `i64` revision, neither indexed nor rendered, whose only home is the record
/// blob. Two widths in the blob for the same reason the hot tail's fixture has three: a tag slip
/// must be a type mismatch, not a plausible value.
const RECORD_SCHEMA_TOML: &str = r#"
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
name       = "tier"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  bronze = 1
  silver = 2
  gold   = 3

[[attribute]]
name       = "band"
type       = "category"
render     = true
vocabulary = "band"

[[attribute]]
name = "note"
type = "keyword"

[[attribute]]
name = "revision"
type = "i64"

# **A `public` category with neither flag.** §4.2's entity-space floor belongs to a category's
# *readers* — `/v1/categories` and the `derived` gate — so this shape has no reader, no floor,
# and no home but the blob. Declared last so the existing field tags do not move.

[[attribute]]
name       = "tier"
type       = "category"
vocabulary = "tier"
"#;

fn note_of(source: u64) -> String {
    format!("note-{source:05}")
}

fn revision_of(source: u64) -> i64 {
    40_000 + source as i64
}

fn tier_of(entity: u64) -> &'static str {
    match entity % 3 {
        0 => "bronze",
        1 => "silver",
        _ => "gold",
    }
}

fn tier_code_of(entity: u64) -> u8 {
    (entity % 3) as u8 + 1
}

/// The blob row the fixture's generation functions predict for `source`: `note` is declared at
/// position 1 and `revision` at position 2, and the field tag **is** the declared position
/// (records §3) — the same identity the build's blob stage and the flush's extent writer share.
fn record_fields_of(source: u64) -> Vec<mosaica_filter::RecordField> {
    vec![
        mosaica_filter::RecordField {
            tag: 1,
            value: mosaica_filter::RecordValue::Utf8(note_of(source)),
        },
        mosaica_filter::RecordField {
            tag: 2,
            value: mosaica_filter::RecordValue::I64(revision_of(source)),
        },
        mosaica_filter::RecordField {
            tag: 3,
            value: mosaica_filter::RecordValue::U8(tier_code_of(source)),
        },
        mosaica_filter::RecordField {
            tag: 4,
            value: mosaica_filter::RecordValue::U64(source),
        },
    ]
}

fn write_points_with_record_columns(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("band", DataType::Utf8, false),
        Field::new("note", DataType::Utf8, false),
        Field::new("revision", DataType::Int64, false),
        Field::new("tier", DataType::Utf8, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 37) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 53) % 1000) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| band_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| note_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(arrow::array::Int64Array::from(
                ids.iter().map(|e| revision_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| tier_of(*e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_record_fixture(out: &Path, tmp: &Path, n: u64) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points_with_record_columns(&points, n);
    write_pairs_n(&pairs, n);
    let schema_path = tmp.join("schema.toml");
    std::fs::write(&schema_path, RECORD_SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &std::collections::HashMap::new())
        .map(|c| c.schema)
        .expect("the record fixture schema parses");
    let args = BuildArgs {
        views: vec![mosaica_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: mosaica_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: mosaica_build::config::AccessInput::relation(pairs),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: mosaica_build::config::AttributeSource::over(
            points.clone(),
            &with_id(schema.clone()),
        ),
        out: out.to_path_buf(),
        limit: None,
        strict: false,
        identity_key: test_key(),
        shard_id: 0,
        layers: Vec::new(),
        layer_inputs: Vec::new(),
        scoped_layers: Default::default(),
        emit_oracle_pairs: false,
        batch_items: None,
        memory_budget: None,
        band_rows: None,
        schema: with_id(schema),
    };
    build(&args).expect("a build with blob-resident columns succeeds");
}

/// The partition's side-manifest, as the serving path holds it.
fn side_manifest(root: &Path) -> mosaica_store::manifest::SegmentsManifest {
    let bundle = open_bundle(root).expect("the bundle opens");
    bundle.partitions["default"].manifest.clone()
}

/// The record stack exactly as drill-down will open it: the base blob under `attrs/record` plus
/// every extent the manifest's `record_extents` names, oldest first.
fn record_stack(root: &Path) -> mosaica_filter::RecordStack {
    let prefix_dir = root.join(current_prefix(root));
    let manifest = side_manifest(root);
    let base = prefix_dir.join("partitions/default/attrs/record");
    let extents: Vec<mosaica_filter::RecordExtentPaths> = manifest
        .record_extents
        .iter()
        .map(|e| mosaica_filter::RecordExtentPaths {
            blocks: prefix_dir.join(&e.blocks),
            hasrow: prefix_dir.join(&e.hasrow),
            directory: prefix_dir.join(&e.directory),
        })
        .collect();
    mosaica_filter::RecordStack::open(Some(&base), &extents, mosaica_filter::Access::Read)
        .expect("the stack opens fail-closed over every layer the manifest names")
}

/// One ingest row for the record fixture: `band` code, blob-resident `note` and `revision`.
fn record_row(engine: &Engine, note: &str, revision: i64) -> UnallocatedRow {
    UnallocatedRow {
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 5.0,
        y: 5.0,
        scalars: vec![
            WalScalar::U8(2),
            WalScalar::Utf8(note.to_string()),
            WalScalar::I64(revision),
            WalScalar::U8(3),
        ],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    }
}

/// **A flush's record extent round-trips through the stack**, beside the build's base: the
/// manifest names one `RecordExtent` whose three files are digested, and `RecordStack` answers a
/// built entity from the base and the flushed entity from the extent — the same read drill-down
/// performs.
#[test]
fn a_flushed_record_extent_round_trips_through_the_stack() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_record_fixture(&root, tmp.path(), 512);
    let engine = engine_over(tmp.path(), &root, config());

    // The base first: every built entity's blob row is the generation functions' values, joined
    // source → entity through the sidecar exactly as the hot-tail cases join.
    let entity_of_source = source_to_new_map(&root, "v00000");
    let stack = record_stack(&root);
    for source in [0u64, 1, 255, 511] {
        let entity = u32::try_from(entity_of_source[&source]).unwrap();
        assert_eq!(
            stack.fields_of(entity).expect("a clean read"),
            Some(record_fields_of(source)),
            "source {source}'s built blob row"
        );
    }

    let entity = engine
        .ingest_rows(
            vec![record_row(&engine, "the-flushed-note", 77)],
            "batch-record-1".to_string(),
            [0u8; 32],
        )
        .expect("an ingest carrying blob-resident values is accepted")[0];
    publish_buffered(&engine);

    // **Before the engine is dropped**: a published record extent that no *live* stack holds
    // answers no drill-down. The manifest entry below makes the bytes reachable to a reopen; this
    // assertion is the other half, and reading only the reopened stack is what let the flush
    // publish an extent it never composed — every entity flushed since process start showing its
    // blob-resident fields as absent, silently, until the next fold.
    let session = engine.authorise(&full_coverage_credential()).unwrap();
    let flushed_id = engine.mosaica_id_of(entity).unwrap();
    let served = engine
        .item(&session, flushed_id)
        .expect("drill-down on a flushed entity")
        .expect("the flushed entity is visible");
    let note = served
        .fields
        .iter()
        .find(|f| f.name == "note")
        .unwrap_or_else(|| {
            panic!(
                "the flushed item carries no `note` field: {:?}",
                served.fields
            )
        });
    assert_eq!(
        note.value,
        mosaica_engine::ScalarOut::Utf8("the-flushed-note".to_string()),
        "the live generation serves the flushed blob row"
    );

    drop(engine);

    let manifest = side_manifest(&root);
    assert_eq!(manifest.record_extents.len(), 1, "one extent per flush");
    let extent = &manifest.record_extents[0];
    for rel in [&extent.blocks, &extent.hasrow, &extent.directory] {
        assert!(
            manifest.files.contains_key(rel),
            "an extent file the manifest names but does not digest: {rel}"
        );
    }

    let stack = record_stack(&root);
    let entity = u32::try_from(entity.raw()).unwrap();
    assert_eq!(
        stack.fields_of(entity).expect("a clean read"),
        Some(vec![
            mosaica_filter::RecordField {
                tag: 1,
                value: mosaica_filter::RecordValue::Utf8("the-flushed-note".to_string()),
            },
            mosaica_filter::RecordField {
                tag: 2,
                value: mosaica_filter::RecordValue::I64(77),
            },
            // The `public` category with neither flag: no hot column, no entity-space structure,
            // so the blob is its only home and the flush owes it exactly as the build does.
            mosaica_filter::RecordField {
                tag: 3,
                value: mosaica_filter::RecordValue::U8(3),
            },
        ]),
        "the flushed row's blob values are what was ingested"
    );
    // And the base still answers through the same stack — the layered read, not one layer's.
    let built = u32::try_from(entity_of_source[&0]).unwrap();
    assert_eq!(
        stack.fields_of(built).expect("read"),
        Some(record_fields_of(0))
    );
    stack.self_check().expect("every layer's addressing");
}

/// **Rule S and Rule F, at the blob's bytes** (write-path §5.4). A suppression — and an accepted
/// deletion, and an unrelated flush — changes not one byte of a published record extent; the
/// compaction fold is the only operation that removes a deleted row, and what it publishes holds
/// the suppressed row intact while the deleted one is **byte-absent**.
///
/// Byte-absence is asserted by exhaustive walk rather than by decompression in this test: rows
/// tile their blocks exactly (`for_each_row` refuses anything else), so once every decoded row is
/// a surviving entity's expected fields, every block byte is accounted for and none of them is
/// the deleted row's. The unit half (`mosaica-filter-write`'s
/// `a_blanked_rows_bytes_are_not_in_the_folded_blob`) additionally greps the decompressed frames.
#[test]
fn a_suppression_touches_no_blob_byte_and_only_the_fold_removes_a_deletion() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_record_fixture(&root, tmp.path(), 64);
    let engine = engine_over(tmp.path(), &root, config());

    let entities = engine
        .ingest_rows(
            vec![
                record_row(&engine, "the-suppressed-prose", 1),
                record_row(&engine, "the-deleted-prose", 2),
                record_row(&engine, "the-kept-prose", 3),
            ],
            "batch-deny".to_string(),
            [1u8; 32],
        )
        .expect("accepted");
    publish_buffered(&engine);

    let manifest = side_manifest(&root);
    assert_eq!(manifest.record_extents.len(), 1);
    let prefix_dir = root.join(current_prefix(&root));
    let extent = manifest.record_extents[0].clone();
    let extent_files = || -> Vec<Vec<u8>> {
        [&extent.blocks, &extent.hasrow, &extent.directory]
            .iter()
            .map(|rel| std::fs::read(prefix_dir.join(rel)).expect("an extent file"))
            .collect()
    };
    let before = extent_files();

    // Rule S: the suppression is in force the moment it is accepted, and it touches no artefact.
    engine
        .accept_change(entities[0], ChangeOp::Suppress)
        .expect("suppressed");
    assert_eq!(before, extent_files(), "a suppression changed a blob byte");
    // Rule F's first half: an accepted deletion stands in the overlay and touches no artefact
    // either — its removal belongs to the fold alone.
    engine
        .accept_change(entities[1], ChangeOp::Delete)
        .expect("deleted");
    assert_eq!(
        before,
        extent_files(),
        "an accepted deletion changed a blob byte"
    );
    // An unrelated publication leaves the extent alone too: a flush appends its own layer.
    engine
        .ingest_rows(
            vec![record_row(&engine, "a-later-note", 4)],
            "batch-later".to_string(),
            [2u8; 32],
        )
        .expect("accepted");
    publish_buffered(&engine);
    assert_eq!(
        before,
        extent_files(),
        "another flush changed the first extent's bytes"
    );

    // Rule F's second half: the fold executes the deletion — and nothing else.
    fold(&engine);
    drop(engine);

    let manifest = side_manifest(&root);
    assert!(
        manifest.record_extents.is_empty(),
        "the fold consumed every record extent into the new base: {:?}",
        manifest.record_extents
    );
    let stack = record_stack(&root);
    let suppressed = u32::try_from(entities[0].raw()).unwrap();
    let deleted = u32::try_from(entities[1].raw()).unwrap();
    let kept = u32::try_from(entities[2].raw()).unwrap();
    assert_eq!(
        stack.fields_of(suppressed).expect("read"),
        Some(vec![
            mosaica_filter::RecordField {
                tag: 1,
                value: mosaica_filter::RecordValue::Utf8("the-suppressed-prose".to_string()),
            },
            mosaica_filter::RecordField {
                tag: 2,
                value: mosaica_filter::RecordValue::I64(1),
            },
            mosaica_filter::RecordField {
                tag: 3,
                value: mosaica_filter::RecordValue::U8(3),
            },
        ]),
        "the suppressed row folds through intact — a later unsuppress reveals exactly this"
    );
    assert!(
        !stack
            .has_row(deleted)
            .expect("a served blob holds its has-row bitmap"),
        "the deleted entity is out of has-row"
    );
    assert_eq!(stack.fields_of(deleted).expect("a clean read"), None);
    assert!(stack.fields_of(kept).expect("read").is_some());

    // The exhaustive walk over the folded base: every row is a surviving entity's, so the deleted
    // row's bytes are in no block (see this test's doc for why the walk is the byte argument).
    let base = mosaica_filter::RecordBlob::open_dir(
        &root
            .join(current_prefix(&root))
            .join("partitions/default/attrs/record"),
        mosaica_filter::Access::Read,
    )
    .expect("the folded base opens");
    let mut saw_deleted = false;
    base.for_each_row(&mut |entity, fields| {
        assert_ne!(
            entity, deleted,
            "the deleted entity has a row in the folded blob"
        );
        if fields
            .iter()
            .any(|f| f.value == mosaica_filter::RecordValue::Utf8("the-deleted-prose".to_string()))
        {
            saw_deleted = true;
        }
        Ok(())
    })
    .expect("the folded blob walks clean");
    assert!(!saw_deleted, "the deleted prose survives in some other row");
}

/// **The record axis coalesces**: a window of flush extents collapses to one manifest entry whose
/// answers are the layers' own — the differential — while a coalesce retires nothing and moves no
/// geometry.
#[test]
fn a_coalesce_collapses_record_extents_and_every_row_still_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_record_fixture(&root, tmp.path(), 64);
    let engine = engine_over(
        tmp.path(),
        &root,
        EngineConfig {
            // Long, so every flush is one this test asked for — `tests/coalesce.rs`'s posture.
            flush_max_age_secs: 3600,
            // **The row trigger off.** This cell drives publication itself — it pins `B`
            // by flushing and waiting, so a trigger that published on its own would
            // measure a different buffer depth than the one the sweep set.
            flush_max_items: usize::MAX,
            ..config()
        },
    );
    engine.set_merge_for_test(false);

    // The policy's width: eight extents select a window.
    let mut ingested = Vec::new();
    for i in 0..8u64 {
        let entity = engine
            .ingest_rows(
                vec![record_row(
                    &engine,
                    &format!("coalesced-note-{i}"),
                    i as i64,
                )],
                format!("batch-co-{i}"),
                [i as u8; 32],
            )
            .expect("accepted")[0];
        ingested.push((entity, format!("coalesced-note-{i}"), i as i64));
        let flushes = engine.write_executor_stats().flushes;
        engine.request_flush();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while engine.write_executor_stats().flushes == flushes {
            assert!(
                std::time::Instant::now() < deadline,
                "the flush never published"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    assert_eq!(
        side_manifest(&root).record_extents.len(),
        8,
        "one record extent per flush before the coalesce"
    );
    // The base plus one layer per extent, in the stack the *running* process reads from.
    assert_eq!(
        engine.generation().filter_columns.record_layers(),
        9,
        "the live stack composes each flush's extent as it publishes"
    );

    tick_until(
        &engine,
        "the record extents to coalesce",
        std::time::Duration::from_secs(60),
        || engine.generation().filter_columns.record_layers() < 9,
    );

    // **The live stack shrank with the manifest, before any restart.** The blob's layers ride on
    // `Arc`s from one generation to the next, so a publication that edited only the manifest left
    // this reader probing the eight extents the coalesce had just consumed — until the process
    // restarted, and with no wrong answer to find it by, the layers being disjoint in entity space
    // (I9). The count is the assertion because the count is the cost: a miss walks every layer,
    // and the whole point of the pass is that there are fewer of them.
    assert_eq!(
        engine.generation().filter_columns.record_layers(),
        2,
        "the base plus the one coalesced extent, re-derived from the rebased manifest"
    );
    // And it answers — a re-derived stack that opened the wrong files would be caught here rather
    // than at the restart below.
    for (entity, note, _) in &ingested {
        let entity = u32::try_from(entity.raw()).unwrap();
        let fields = engine
            .generation()
            .filter_columns
            .records()
            .fields_of(entity)
            .expect("the live stack reads")
            .expect("every ingested entity has a blob row");
        assert!(
            fields
                .iter()
                .any(|f| f.value == mosaica_filter::RecordValue::Utf8(note.clone())),
            "entity {entity} lost its note to the coalesce's live publication"
        );
    }
    drop(engine);

    let manifest = side_manifest(&root);
    assert_eq!(
        manifest.record_extents.len(),
        1,
        "eight extents became one: {:?}",
        manifest.record_extents
    );
    let stack = record_stack(&root);
    for (entity, note, revision) in &ingested {
        let entity = u32::try_from(entity.raw()).unwrap();
        assert_eq!(
            stack.fields_of(entity).expect("read"),
            Some(vec![
                mosaica_filter::RecordField {
                    tag: 1,
                    value: mosaica_filter::RecordValue::Utf8(note.clone()),
                },
                mosaica_filter::RecordField {
                    tag: 2,
                    value: mosaica_filter::RecordValue::I64(*revision),
                },
                mosaica_filter::RecordField {
                    tag: 3,
                    value: mosaica_filter::RecordValue::U8(3),
                },
            ]),
            "entity {entity} answers differently through the coalesced extent"
        );
    }
    stack
        .self_check()
        .expect("the coalesced extent's addressing");
}
