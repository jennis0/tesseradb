//! `point_visibility = { field }`: a point's access terms read from a column of the view's own
//! source, and `public` reserved at term 0 (`configuration.md` §1, `per-point-attributes.md` §3.8).
//!
//! **The one case in this stage that changes what a request computes**, so it is asserted end to
//! end rather than at the reader: a bundle is built from a `list<string>` column and then
//! *authorised against*, because the properties at issue are properties of a principal's mask.
//! Four of them, each the fail-closed half of a plausible misreading:
//!
//! - a **null** value and an **empty list** both mean *no access terms*, which means visible to no
//!   principal — never unrestricted. Where a `default` is declared it fills those rows, and where
//!   that default is a label nobody holds they stay invisible;
//! - **filling never overrides**: a point carrying terms of its own keeps exactly those, and is not
//!   also given the default. A point's terms are disjunctive, so a label added to one can only
//!   widen it;
//! - **terms are trimmed**, so ` ir:analyst ` and `ir:analyst` are one term rather than two that no
//!   credential spells the same way;
//! - **`public` reaches every principal by construction**, including a credential carrying no
//!   descriptors at all — and it is added inside the engine rather than granted or plugged in.

mod common;

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, ListArray, StringArray, UInt64Array};
use arrow::buffer::OffsetBuffer;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::{extent, open_engine, open_engine_publishing, test_key, wait_until};
use tessera_engine::{
    Engine, IngestRequest, ItemsRequest, PageEnd, RecordsHead, RecordsLimits, RecordsSink,
    Session, SinkResult,
};
use tessera_lifecycle::{ChangeOp, IngestRow};
use tessera_types::TesseraId;
use tessera_build::config::{AccessInput, AccessSource};
use tessera_build::{build, BuildArgs};

/// Five points, one per shape a row can take: two terms, null, empty, a term needing a trim, and
/// one term.
fn access_of(e: u64) -> Option<Vec<&'static str>> {
    match e {
        0 => Some(vec!["ir:analyst", "ir:legal"]),
        1 => None,
        2 => Some(vec![]),
        3 => Some(vec!["  public  "]),
        _ => Some(vec![" ir:analyst"]),
    }
}

const N: u64 = 5;

/// The points source, carrying its own `list<string>` access column.
fn write_points(path: &Path, access: impl Fn(u64) -> Option<Vec<&'static str>>) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new(
            "categories",
            DataType::List(Arc::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();

    let mut offsets: Vec<i32> = vec![0];
    let mut flat: Vec<&str> = Vec::new();
    let mut present: Vec<bool> = Vec::new();
    for &e in &ids {
        match access(e) {
            None => present.push(false),
            Some(terms) => {
                present.push(true);
                flat.extend(terms);
            }
        }
        offsets.push(flat.len() as i32);
    }
    let values: ArrayRef = Arc::new(StringArray::from(flat));
    let list = ListArray::new(
        Arc::new(Field::new("item", DataType::Utf8, true)),
        OffsetBuffer::new(offsets.into()),
        values,
        Some(present.into()),
    );

    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(list),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn args(points: &Path, out: &Path, default: Option<&str>) -> BuildArgs {
    BuildArgs {
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.to_path_buf(),
            point_fields: Default::default(),
            select: None,
            access: AccessInput {
                source: AccessSource::Field("categories".to_string()),
                default: default.map(str::to_string),
            },
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: Vec::new(),
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
        schema: Default::default(),
    }
}

/// How many of the corpus's points a credential carrying `terms` may see.
fn visible(dir: &Path, terms: &[&str]) -> u64 {
    let engine = open_engine(
        &dir.join("bundle"),
        &dir.join("cache"),
        &dir.join("wal.log"),
    );
    let credential = format!(
        "{{\"terms\": [{}]}}",
        terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let session = engine
        .authorise(credential.as_bytes())
        .expect("a credential of known descriptors authorises");
    let cardinality = session.fragment_at_authorise_for_test().view().cardinality();
    cardinality
}

/// The whole of the field route's semantics, in one corpus, under `default = "public"`.
#[test]
fn a_field_sourced_view_masks_on_the_terms_its_rows_carry() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), access_of);
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        Some("public"),
    ))
    .expect("a field-sourced view builds");

    // A credential carrying nothing at all still holds `public`, which is added by the engine and
    // not by the credential: points 1 and 2 took the fill, point 3 wrote the label itself after a
    // trim. Points 0 and 4 carry their own terms and are **not** also given the default.
    assert_eq!(visible(dir.path(), &[]), 3, "public, and only public");
    // The trim: the credential spells the label without the padding the file carried.
    assert_eq!(visible(dir.path(), &["public"]), 3, "one term, not two");
    // Two more, and no double-counting of the point that carries both descriptors.
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 5);
    assert_eq!(visible(dir.path(), &["ir:legal"]), 4);
    // A descriptor no point carries adds nothing, and takes nothing away.
    assert_eq!(visible(dir.path(), &["ir:nobody"]), 3);
}

/// **Null is not unrestricted.** Under a default no principal holds, the rows it fills are visible
/// to nobody — which is the reading that makes a fill safe in the first place.
#[test]
fn a_null_row_is_visible_to_no_principal_when_the_default_is_not_public() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), access_of);
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        Some("ir:sealed"),
    ))
    .expect("a field-sourced view builds");

    // Only point 3, which wrote `public` itself.
    assert_eq!(visible(dir.path(), &[]), 1);
    // The two filled rows are reachable by the declared label and by nothing else.
    assert_eq!(visible(dir.path(), &["ir:sealed"]), 3);
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 3);
}

/// **No default, and a null or empty row, is a refusal** (decision 0133): the same corpus that
/// the two tests above fill is refused when the view declares nothing to fill with, naming the
/// count of such rows and the view, and nothing is written.
#[test]
fn a_null_row_is_refused_naming_the_count_when_no_default_is_declared() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), access_of);
    let error = build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        None,
    ))
    .expect_err("a null row with no declared default is refused");
    let message = error.to_string();
    // Points 1 (null) and 2 (empty list) are the two unlabelled rows of `access_of`.
    assert!(
        message.contains("view 's0': 2 point row(s) carry a null or empty access label"),
        "{message}"
    );
    assert!(
        message.contains("declares no `point_visibility.default`"),
        "{message}"
    );
    assert!(
        !dir.path().join("bundle").join("MANIFEST.json").exists(),
        "a refused corpus writes no manifest"
    );
}

/// A view declaring no default still builds a corpus whose every row carries a label: the
/// refusal is about the rows, and a corpus with none to refuse is unchanged by it.
#[test]
fn a_fully_labelled_corpus_builds_without_a_default() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), |e| match access_of(e) {
        Some(terms) if !terms.is_empty() => Some(terms),
        _ => Some(vec!["ir:sealed"]),
    });
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        None,
    ))
    .expect("a corpus with a label on every row builds without a default");
    // Points 1 and 2 were relabelled `ir:sealed`; 0 and 4 carry `ir:analyst` of their own and 3
    // carries `public` alone.
    assert_eq!(visible(dir.path(), &[]), 1);
    assert_eq!(visible(dir.path(), &["ir:sealed"]), 3);
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 3);
}

/// A view declaring only a `default` — the corpus with no permission model — gives every point
/// that one label and nothing else.
#[test]
fn a_default_alone_gives_every_point_the_declared_label() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), access_of);
    let mut args = args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        Some("public"),
    );
    args.views[0].access.source = AccessSource::Default;
    build(&args).expect("a default-only view builds");
    assert_eq!(visible(dir.path(), &[]), N);
}

/// **A term holding a comma is written in quotes**, and is then one term: it reaches exactly the
/// principal who holds that whole string, never the holder of either half. Written bare, the comma
/// is refused, so a build never reads one label as two terms.
#[test]
fn a_quoted_term_containing_a_comma_is_one_term() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), |e| {
        if e == 2 {
            Some(vec!["\"ir:analyst,ir:legal\""])
        } else {
            access_of(e)
        }
    });
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        Some("public"),
    ))
    .expect("a quoted term carrying a comma builds");

    // Points 1 (null, filled) and 3 (`public`) and nothing else.
    assert_eq!(visible(dir.path(), &[]), 2);
    // Neither half reaches point 2.
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 4);
    assert_eq!(visible(dir.path(), &["ir:legal"]), 3);
    // The whole string is the term, and it reaches its one point.
    assert_eq!(visible(dir.path(), &["ir:analyst,ir:legal"]), 3);

    let bare = tempfile::tempdir().unwrap();
    write_points(&bare.path().join("points.parquet"), |e| {
        if e == 2 {
            Some(vec!["ir:analyst,ir:legal"])
        } else {
            access_of(e)
        }
    });
    build(&args(
        &bare.path().join("points.parquet"),
        &bare.path().join("bundle"),
        Some("public"),
    ))
    .expect_err("a bare comma is not an access expression");
}

/// A plain `string` column is the same declaration for a point carrying one term.
#[test]
fn a_plain_string_access_column_is_one_term_per_point() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("points.parquet");
    let schema = Arc::new(Schema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("categories", DataType::Utf8, true),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let values: Vec<Option<&str>> = vec![Some("ir:analyst"), None, Some(""), Some("public"), None];
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(values)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(&path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    build(&args(&path, &dir.path().join("bundle"), Some("public"))).expect("a string column builds");
    // The one point carrying a term of its own is not also given the default; the empty string is
    // no term at all, so that row is filled.
    assert_eq!(visible(dir.path(), &[]), 4);
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 5);
}

/// Five points labelled with each shape of expression: a conjunction, a conjunction over a
/// disjunction, a list holding a term and a conjunction, `public`, and a disjunction of terms.
fn expressions_of(e: u64) -> Option<Vec<&'static str>> {
    match e {
        0 => Some(vec!["ir:analyst&ir:legal"]),
        1 => Some(vec!["(ir:analyst|ir:audit)&eu"]),
        2 => Some(vec!["ir:legal", "ir:audit&eu"]),
        3 => Some(vec!["public"]),
        _ => Some(vec!["ir:analyst|ir:legal"]),
    }
}

fn credential(terms: &[&str]) -> Vec<u8> {
    serde_json::json!({ "terms": terms }).to_string().into_bytes()
}

/// **A label holding a conjunction admits only a principal holding every term it needs**, and a
/// list of labels admits a principal satisfying any one of them. Each count is the authorised set
/// of a credential, so it is the set every count, sample and label a viewer is served is drawn
/// from.
#[test]
fn a_conjunction_admits_only_a_principal_satisfying_it() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), expressions_of);
    build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        None,
    ))
    .expect("a corpus labelled with expressions builds");

    assert_eq!(visible(dir.path(), &[]), 1, "point 3, `public`");
    assert_eq!(visible(dir.path(), &["ir:analyst"]), 2, "points 3 and 4");
    assert_eq!(visible(dir.path(), &["ir:analyst", "ir:legal"]), 4, "0, 2, 3 and 4");
    assert_eq!(visible(dir.path(), &["ir:audit", "eu"]), 3, "1, 2 and 3");
    assert_eq!(visible(dir.path(), &["ir:analyst", "eu"]), 3, "1, 3 and 4");
    assert_eq!(visible(dir.path(), &["eu"]), 1, "half of a conjunction admits nothing");

    // A credential naming a conjunction's own index key holds no term, so it sees `public` alone.
    let engine = open_engine(
        &dir.path().join("bundle"),
        &dir.path().join("cache"),
        &dir.path().join("wal.log"),
    );
    let session = engine
        .authorise(&credential(&["\u{0}ir:analyst&ir:legal", "\u{0}ir:analyst"]))
        .unwrap();
    assert_eq!(
        session.fragment_at_authorise_for_test().view().cardinality(),
        1
    );
}

/// **A label that is not an access expression refuses the build**, before anything is written.
#[test]
fn a_label_that_is_not_an_expression_refuses_the_build() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), |e| match e {
        2 => Some(vec!["ir:analyst&ir:legal|eu"]),
        _ => expressions_of(e),
    });
    let error = build(&args(
        &dir.path().join("points.parquet"),
        &dir.path().join("bundle"),
        None,
    ))
    .expect_err("mixing `&` and `|` without brackets is refused");
    assert!(error.to_string().contains("ir:analyst&ir:legal|eu"), "{error}");
    assert!(!dir.path().join("bundle").join("MANIFEST.json").exists());
}

/// Collects the pages of one bulk read.
#[derive(Default)]
struct Pages(Vec<RecordBatch>);

impl RecordsSink for Pages {
    fn head(&mut self, _: &RecordsHead) -> SinkResult {
        Ok(())
    }

    fn page(&mut self, batch: &RecordBatch, _: &PageEnd) -> SinkResult {
        self.0.push(batch.clone());
        Ok(())
    }
}

/// Every row a bulk read of `s0` serves `session`: its `tessera_id` and its `labels` column.
fn rows(engine: &Engine, session: &Session) -> Vec<(TesseraId, Vec<String>)> {
    let system = ["labels".to_string()];
    let mut pages = Pages::default();
    engine
        .items_stream(
            session,
            ItemsRequest {
                view: "s0",
                fields: &[],
                system_fields: &system,
                filter: None,
                keep_unmatched: false,
                count: false,
                order: None,
                page_rows: None,
                pages: None,
                cursor: None,
                limits: RecordsLimits {
                    max_page_rows: 1024,
                    max_page_bytes: 1 << 20,
                    response_bytes: 1 << 24,
                    response_time: std::time::Duration::from_secs(60),
                },
                cancel: None,
            },
            &mut pages,
        )
        .expect("the read is served");
    let mut rows = Vec::new();
    for batch in &pages.0 {
        let ids = batch.column(0).as_any().downcast_ref::<UInt64Array>().unwrap();
        let labels = batch
            .column_by_name("tessera:labels")
            .and_then(|c| c.as_any().downcast_ref::<ListArray>())
            .expect("a labels column");
        for (row, &id) in ids.values().iter().enumerate() {
            let row = labels.value(row);
            let row = row.as_any().downcast_ref::<StringArray>().unwrap();
            let labels = row.iter().map(|l| l.unwrap().to_string()).collect();
            rows.push((TesseraId::new(id), labels));
        }
    }
    rows
}

/// The `labels` column a bulk read of `s0` serves `session` for the item `id`.
fn labels_column(engine: &Engine, session: &Session, id: TesseraId) -> Vec<String> {
    rows(engine, session)
        .into_iter()
        .find_map(|(served, labels)| (served == id).then_some(labels))
        .unwrap_or_else(|| panic!("the read serves no row for {id:?}"))
}

/// One batch of rows creating items at `positions`, each with its labels.
fn create(engine: &Engine, batch: &str, rows: &[(&[&str], (f64, f64))]) -> Vec<TesseraId> {
    let mut body_hash = [0u8; 32];
    body_hash[..batch.len()].copy_from_slice(batch.as_bytes());
    let rows = rows
        .iter()
        .map(|(labels, at)| IngestRow {
            tessera_id: None,
            labels: Some(labels.iter().map(|l| l.as_bytes().to_vec()).collect()),
            position: Some(*at),
            scalars: Vec::new(),
            scoped: Vec::new(),
            omitted: Vec::new(),
        })
        .collect();
    engine
        .ingest(IngestRequest {
            batch_id: batch.to_string(),
            body_hash,
            view: Some("s0".to_string()),
            rows,
            artifacts: Default::default(),
            strict: false,
            tessera_id_column: false,
        })
        .unwrap_or_else(|e| panic!("{batch} is accepted: {e}"))
        .tessera_ids
        .into_iter()
        .map(|id| id.expect("an accepted row has a tessera_id"))
        .collect()
}

/// **A label ingested into a running service is read by the rule the build reads it by**: a
/// conjunction is evaluated from a credential's terms once a flush has published it, the item
/// card names each held term of a label that is a term or a disjunction of terms, and one
/// satisfied clause of each label holding a conjunction, and nothing else, and both survive a
/// restart. A held term that appears only inside a conjunction is not named on its own.
#[test]
fn an_ingested_conjunction_is_served_as_a_built_one_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), expressions_of);
    let bundle = dir.path().join("bundle");
    build(&args(&dir.path().join("points.parquet"), &bundle, None)).expect("it builds");

    let engine =
        open_engine_publishing(&bundle, &dir.path().join("cache"), &dir.path().join("wal.log"));
    let ids = create(
        &engine,
        "expressions",
        &[
            (&["eu&(ir:legal|ir:new)"], (10.0, 10.0)),
            (&["ir:new|ir:other"], (20.0, 20.0)),
            (&["eu&(ir:legal|ir:new)", "ir:new|ir:secret", "x&y"], (40.0, 40.0)),
        ],
    );
    let refused = engine.ingest(IngestRequest {
        batch_id: "refused".to_string(),
        body_hash: [1u8; 32],
        view: Some("s0".to_string()),
        rows: vec![IngestRow {
            tessera_id: None,
            labels: Some(vec![b"ir:new|".to_vec()]),
            position: Some((30.0, 30.0)),
            scalars: Vec::new(),
            scoped: Vec::new(),
            omitted: Vec::new(),
        }],
        artifacts: Default::default(),
        strict: false,
        tessera_id_column: false,
    });
    assert!(refused.is_err(), "a label that does not parse refuses the batch");

    let before = engine.authorise(&credential(&["eu", "ir:new"])).unwrap();
    engine.request_flush();
    wait_until("the flush to publish", std::time::Duration::from_secs(30), || {
        engine.write_executor_stats().flushes >= 1
    });
    // A session authorised before the flush promoted the label is told it is behind, and sees
    // less than its terms admit until it authorises again, never more.
    assert!(before.is_stale(&engine.generation()));
    assert!(engine.item(&before, ids[0]).unwrap().is_none());

    let check = |engine: &Engine| {
        let both = engine.authorise(&credential(&["eu", "ir:new", "ir:secret"])).unwrap();
        let card = engine.item(&both, ids[0]).unwrap().expect("eu&ir:new satisfies it");
        assert_eq!(card.labels, ["eu&ir:new"], "one satisfied clause, held terms only");
        let card = engine.item(&both, ids[1]).unwrap().expect("ir:new satisfies it");
        assert_eq!(card.labels, ["ir:new"]);
        let card = engine.item(&both, ids[2]).unwrap().expect("each of three labels admits it");
        assert_eq!(
            card.labels,
            ["eu&ir:new", "ir:new", "ir:secret"],
            "the held terms of the disjunction, one clause of the conjunction, and nothing of `x&y`"
        );
        let secret = engine.authorise(&credential(&["ir:secret"])).unwrap();
        let card = engine.item(&secret, ids[2]).unwrap().expect("ir:secret satisfies it");
        assert_eq!(card.labels, ["ir:secret"]);

        // `eu` is held, but on this item it appears only inside a conjunction the viewer does
        // not satisfy, so neither the card nor the bulk read names it.
        let secret_eu = engine.authorise(&credential(&["ir:secret", "eu"])).unwrap();
        let card = engine.item(&secret_eu, ids[2]).unwrap().expect("ir:secret satisfies it");
        assert_eq!(card.labels, ["ir:secret"]);
        assert_eq!(labels_column(engine, &secret_eu, ids[2]), ["ir:secret"]);

        // Both operands of the disjunction are held: one clause, the first in byte order.
        let all = engine.authorise(&credential(&["eu", "ir:legal", "ir:new"])).unwrap();
        let card = engine.item(&all, ids[0]).unwrap().expect("eu&ir:legal satisfies it");
        assert_eq!(card.labels, ["eu&ir:legal"]);
        assert_eq!(labels_column(engine, &all, ids[0]), ["eu&ir:legal"]);

        let half = engine.authorise(&credential(&["ir:legal"])).unwrap();
        assert!(engine.item(&half, ids[0]).unwrap().is_none(), "half of the conjunction");
        assert!(engine.item(&half, ids[1]).unwrap().is_none());
        assert!(engine.item(&half, ids[2]).unwrap().is_none());
    };
    check(&engine);
    drop(engine);
    let reopened = open_engine(&bundle, &dir.path().join("cache"), &dir.path().join("wal.log"));
    check(&reopened);
}

/// A bundle of [`expressions_of`]'s points, served by an engine that publishes flushes.
fn expressions_engine() -> (tempfile::TempDir, Engine) {
    let dir = tempfile::tempdir().unwrap();
    write_points(&dir.path().join("points.parquet"), expressions_of);
    let bundle = dir.path().join("bundle");
    build(&args(&dir.path().join("points.parquet"), &bundle, None)).expect("it builds");
    let engine =
        open_engine_publishing(&bundle, &dir.path().join("cache"), &dir.path().join("wal.log"));
    (dir, engine)
}

/// **A `read-all` session sees an item placed after it authorised**, under a term and under a
/// label holding a conjunction that no item carried before, once a flush publishes it, and it is
/// never reported behind. Its item card and `labels` column name what a session holding every
/// term is shown.
#[test]
fn a_read_all_session_sees_items_under_keys_promoted_after_it_authorised() {
    let (_dir, engine) = expressions_engine();
    let all = engine.authorise_all().unwrap();
    assert_eq!(rows(&engine, &all).len(), N as usize, "every built item");

    let ids = create(
        &engine,
        "promoted",
        &[
            (&["ir:brand-new"], (10.0, 10.0)),
            (&["ir:fresh&ir:other"], (20.0, 20.0)),
            (&["eu&(ir:legal|ir:new)", "ir:new|ir:secret"], (30.0, 30.0)),
        ],
    );
    engine.request_flush();
    wait_until("the flush to publish", std::time::Duration::from_secs(30), || {
        engine.write_executor_stats().flushes >= 1
    });
    wait_until("the session to see the new items", std::time::Duration::from_secs(30), || {
        rows(&engine, &all).len() == N as usize + ids.len()
    });
    assert!(!all.is_stale(&engine.generation()));

    let every = engine
        .authorise(&credential(&[
            "eu", "ir:analyst", "ir:audit", "ir:brand-new", "ir:fresh", "ir:legal", "ir:new",
            "ir:other", "ir:secret",
        ]))
        .unwrap();
    for &id in &ids {
        let card = engine.item(&all, id).unwrap().expect("read-all sees it").labels;
        assert_eq!(card, engine.item(&every, id).unwrap().unwrap().labels);
        assert_eq!(labels_column(&engine, &all, id), card);
    }
    let card = |id| engine.item(&all, id).unwrap().unwrap().labels;
    assert_eq!(card(ids[0]), ["ir:brand-new"]);
    assert_eq!(card(ids[1]), ["ir:fresh&ir:other"]);
    assert_eq!(card(ids[2]), ["eu&ir:legal", "ir:new", "ir:secret"]);
}

/// **A deletion and a suppression hide an item from a `read-all` session** from its next request,
/// as from any session, and from a `read-all` session authorised after them.
#[test]
fn deletions_and_suppressions_hide_items_from_a_read_all_session() {
    let (_dir, engine) = expressions_engine();
    let all = engine.authorise_all().unwrap();
    let served = rows(&engine, &all);
    assert_eq!(served.len(), N as usize);
    let (deleted, suppressed) = (served[0].0, served[1].0);
    for (id, op) in [(deleted, ChangeOp::Delete), (suppressed, ChangeOp::Suppress)] {
        let entity = engine.resolve_tessera_ids(&[id]).unwrap()[0].expect("a built item");
        engine.accept_change(entity, op).unwrap();
    }

    for session in [&all, &engine.authorise_all().unwrap()] {
        let left: Vec<TesseraId> = rows(&engine, session).into_iter().map(|(id, _)| id).collect();
        assert_eq!(left.len(), N as usize - 2);
        assert!(!left.contains(&deleted) && !left.contains(&suppressed));
        assert!(engine.item(session, deleted).unwrap().is_none());
        assert!(engine.item(session, suppressed).unwrap().is_none());
    }
}

/// **Every `read-all` session at one watermark is served one fragment**, built once.
#[test]
fn read_all_sessions_share_one_fragment() {
    let (_dir, engine) = expressions_engine();
    let _first = engine.authorise_all().unwrap();
    let before = engine.generation_status();
    let _second = engine.authorise_all().unwrap();
    let after = engine.generation_status();
    assert_eq!(after.fragment_cache_rebuilds, before.fragment_cache_rebuilds);
    assert_eq!(after.fragment_cache.entries, before.fragment_cache.entries);
}
