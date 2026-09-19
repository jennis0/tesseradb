//! **A text column's layers are bounded between folds** — the sixth coalesce axis
//! (`filter-index.md` §5.2, `records-and-search.md` §7).
//!
//! Every flush carrying prose publishes a text layer of its own: a dictionary, postings over it,
//! and a presence bitmap. Nothing merged them until this axis existed, so a `match` paid one
//! dictionary resolve and one posting read **per token per layer**, growing linearly in the flush
//! count with only the nightly fold reducing it. This module is where that bound has symptoms.
//!
//! The arc is the attribute axis's: build a corpus with prose, flush a window's worth of ingests,
//! and let the pass fire. What is asserted is that the window becomes one layer, that the *live*
//! generation serves from it — a manifest edit the running process ignores is a bound that arrives
//! at the next restart — and that every word still names exactly the entities it did.
//!
//! **The trap is the renumbering.** The merged dictionary is a new key set: a term that was ordinal
//! 0 in one input and ordinal 3 in another is one term of the output at neither position. Nothing
//! outside the three files holds a text ordinal, so no remap of anything else is owed — but a merge
//! that mispaired a term with a posting would answer confidently from the wrong word, and no
//! cardinality anywhere would move. The prose below is chosen so that every layer disagrees with
//! every other about which term sits where: each flush carries a word no other flush does, `shared`
//! is in all of them, and each flush's own first term sorts before the others'.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::Engine;
use tessera_lifecycle::wal::WalScalar;
use tessera_lifecycle::UnallocatedRow;
use tessera_store::read::open_bundle;
use tessera_types::AttrLocalId;

/// Built items — small, because the built layer is not what this module is about.
const N: u64 = 16;

/// The coalesce policy's default width: the window it takes, and therefore the number of flushes
/// this module has to publish before the pass fires at all.
const COALESCE_WIDTH: usize = 8;

const SCHEMA_TOML: &str = r#"
[[attribute]]
name     = "prose"
type     = "text"
index    = true
analyser = "unicode"
"#;

fn built_prose(source: u64) -> String {
    format!("shared builtword b{source:02}")
}

/// One flushed item's prose. **Each flush's dictionary disagrees with every other's**, which is the
/// whole of what a renumbering merge has to get right:
///
/// * `shared` is in every layer, so the merged term's posting is a union rather than a copy;
/// * `a{i}word` is the layer's own first term and no other layer holds it, so an ordinal reused
///   across layers names a different word rather than nothing;
/// * `f{i}` is carried by exactly one item, which makes a single-item answer a mispaired posting
///   cannot fake.
fn flushed_prose(i: usize) -> String {
    format!("a{i}word shared f{i:02}")
}

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("prose", DataType::Utf8, false),
    ]));
    let ids: Vec<u64> = (0..n).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids.clone())),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 37) % 500) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                ids.iter()
                    .map(|e| ((e * 53) % 500) as f64)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| built_prose(*e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_text_fixture(out: &Path, tmp: &Path) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points(&points, N);
    write_pairs_n(&pairs, N);
    let schema_path = tmp.join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &HashMap::new())
        .expect("the text schema parses")
        .schema;
    build(&BuildArgs {
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
        attribute_sources: tessera_build::config::AttributeSource::over(points.clone(), &schema),
        out: out.to_path_buf(),
        limit: None,
        identity_key: test_key(),
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
        schema,
    })
    .expect("a bundle with an indexed text column builds");
}

fn text_extents(root: &Path) -> Vec<tessera_store::manifest::TextExtent> {
    open_bundle(root).expect("the bundle opens").partitions["default"]
        .manifest
        .text_extents
        .clone()
}

/// Ingest one item carrying `prose` and flush it, returning the entity it was allocated.
fn ingest_and_flush(engine: &Engine, root: &Path, tag: &str, prose: String) -> u32 {
    let row = UnallocatedRow {
        external_id: Some(tag.as_bytes().to_vec()),
        view: "s0".to_string(),
        join: None,
        descriptors: vec![b"0".to_vec()],
        x: 10.0,
        y: 10.0,
        scalars: vec![WalScalar::Utf8(prose)],
        terms: engine.resolve_terms(&[b"0".to_vec()]),
        scoped: Vec::new(),
    };
    let entity = engine
        .accept_ingest(vec![row], tag.to_string(), [0u8; 32])
        .expect("an ingest carrying prose is accepted")[0];
    // **Waited on the flush count, not on the extent count.** A coalesce fires between these
    // flushes and *reduces* the number of live text extents, which is the whole point of this
    // module — so a wait for "one more extent than before" hangs the moment the pass it is here to
    // test starts working.
    let before = engine.write_executor_stats().flushes;
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().flushes <= before {
        assert!(
            std::time::Instant::now() < deadline,
            "the flush never published: {:?}",
            text_extents(root)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    entity.raw() as u32
}

fn text_extents_for_column(root: &Path) -> usize {
    text_extents(root)
        .iter()
        .filter(|e| e.column == "prose")
        .count()
}

/// Flush `COALESCE_WIDTH` items and wait for the pass that collapses their layers.
fn flush_a_window(engine: &Engine, root: &Path, from: usize) -> Vec<u32> {
    let coalesces = engine.write_executor_stats().coalesces;
    let entities: Vec<u32> = (from..from + COALESCE_WIDTH)
        .map(|i| ingest_and_flush(engine, root, &format!("fresh-{i}"), flushed_prose(i)))
        .collect();
    // The pass is selected on a tick, and a flush is what drives one.
    engine.request_flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.write_executor_stats().coalesces <= coalesces {
        assert!(
            std::time::Instant::now() < deadline,
            "the coalesce never published: {} text extents live",
            text_extents_for_column(root)
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    entities
}

/// Every term the composed column answers for, with the entities it names — asked of the **live
/// generation**, which is the surface a request meets and the one a manifest-only bound misses.
fn answers(engine: &Engine, terms: &[String]) -> BTreeMap<String, Vec<u32>> {
    let generation = engine.generation();
    let mut candidate = croaring::Bitmap::new();
    candidate.add_range(0..10_000);
    terms
        .iter()
        .map(|term| {
            let answer = generation
                .filter_columns
                .resolve(
                    "prose",
                    &tessera_engine::filter::FilterOperand::Match {
                        query: term.clone(),
                        minimum: None,
                    },
                    &candidate,
                )
                .expect("a match answers");
            (term.clone(), answer.iter().collect())
        })
        .collect()
}

/// The coalesced extent's own dictionary and postings, read straight off disc.
///
/// Read from the artefact rather than through the composition, because the composition unions the
/// layers: a pass that edited the manifest and published a layer holding nothing would still answer
/// every question correctly from the base, and this is what separates the two.
fn extent_index(
    root: &Path,
    extent: &tessera_store::manifest::TextExtent,
) -> BTreeMap<String, Vec<u32>> {
    let prefix = root.join(current_prefix(root));
    let dict =
        tessera_filter::SortedDict::open(&prefix.join(&extent.dict), tessera_filter::Access::Read)
            .expect("the coalesced dictionary opens");
    let postings = tessera_filter::ColumnPostings::open(&prefix.join(&extent.postings), false)
        .expect("the coalesced postings open");
    assert_eq!(
        dict.len(),
        postings.record_count(),
        "the coalesced dictionary and its postings disagree about how many terms there are"
    );
    let mut out = BTreeMap::new();
    dict.walk(|ordinal, key| {
        out.insert(
            key.to_string(),
            postings
                .entities(AttrLocalId::new(ordinal))
                .expect("a posting reads")
                .iter()
                .collect(),
        );
    })
    .expect("the coalesced dictionary walks");
    out
}

/// **A window of text layers becomes one, the live generation serves from it, and every word names
/// exactly the entities it did.**
///
/// Three things a defect here would leave looking right. A merge that dropped a layer answers short
/// on that layer's words alone; one that mispaired a term with a posting answers *another* word's
/// entities; and one that edited only the manifest keeps the read cost it was meant to remove until
/// the next restart. The first two are caught by comparing every term's answer across the pass, the
/// third by asking the live generation how many layers it holds.
#[test]
fn a_window_of_text_layers_becomes_one_and_answers_identically() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("bundle");
    build_text_fixture(&root, dir.path());
    let engine = {
        let mut engine = Engine::open(
            &root,
            &dir.path().join("cache"),
            &dir.path().join("wal.log"),
            tessera_plugin::Passthrough::new(),
            config_uncapped(),
        )
        .expect("the engine opens against the text fixture");
        engine.start_write_executor(8).expect("the executor starts");
        engine.set_background_refresh_for_test(false);
        // The row-space merge is a different pass with a different argument; leaving it on would
        // make a failure here ambiguous between the two.
        engine.set_merge_for_test(false);
        engine
    };

    let entities = flush_a_window(&engine, &root, 0);
    let probes: Vec<String> = std::iter::once("shared".to_string())
        .chain((0..COALESCE_WIDTH).map(|i| format!("a{i}word")))
        .chain((0..COALESCE_WIDTH).map(|i| format!("f{i:02}")))
        .chain(std::iter::once("builtword".to_string()))
        .collect();
    let after_flushes = answers(&engine, &probes);

    // Every flushed item is where the fixture put it, before the pass is asked to preserve it.
    for (i, entity) in entities.iter().enumerate() {
        assert_eq!(
            after_flushes[&format!("f{i:02}")],
            vec![*entity],
            "the fixture's own single-carrier term does not name its item"
        );
    }
    assert_eq!(
        after_flushes["shared"].len(),
        N as usize + COALESCE_WIDTH,
        "`shared` is in every layer and must name every item"
    );

    let live = text_extents(&root);
    let coalesced: Vec<_> = live.iter().filter(|e| e.column == "prose").collect();
    assert_eq!(
        coalesced.len(),
        1,
        "the {COALESCE_WIDTH} text layers did not collapse to one"
    );
    // **The live generation, not only the manifest.** The base plus the coalesced layer is two.
    assert_eq!(
        engine.generation().filter_columns.text_layer_count("prose"),
        Some(2),
        "the process is still serving from the layers the coalesce replaced"
    );

    // The answers are unchanged — the whole obligation of a content-preserving re-encode.
    assert_eq!(answers(&engine, &probes), after_flushes);

    // And the coalesced artefact itself holds the window's terms, so the equality above is not
    // being satisfied by a base that answers everything on its own.
    let index = extent_index(&root, coalesced[0]);
    for i in 0..COALESCE_WIDTH {
        assert_eq!(
            index[&format!("f{i:02}")],
            vec![entities[i]],
            "the coalesced extent lost or mispaired a single-carrier term"
        );
        assert!(index.contains_key(&format!("a{i}word")));
    }
    assert_eq!(
        index["shared"].len(),
        COALESCE_WIDTH,
        "`shared` is in every input layer and its merged posting is their union"
    );
    assert!(
        !index.contains_key("builtword"),
        "the coalesce took the base's index, which belongs to the fold alone"
    );

    // The recursion: a coalesced extent is an entry in the same per-column subsequence and is
    // selected again at the next rung, which is what the per-column selection buys.
    let more = flush_a_window(&engine, &root, COALESCE_WIDTH);
    assert!(engine.write_executor_stats().coalesces >= 2);
    assert!(
        text_extents_for_column(&root) <= 2,
        "a coalesced text extent must coalesce again"
    );
    let probes: Vec<String> = (0..2 * COALESCE_WIDTH)
        .map(|i| format!("f{i:02}"))
        .chain(std::iter::once("shared".to_string()))
        .collect();
    let last = answers(&engine, &probes);
    for (i, entity) in entities.iter().chain(more.iter()).enumerate() {
        assert_eq!(
            last[&format!("f{i:02}")],
            vec![*entity],
            "the second coalesce lost the term item {i} alone carries"
        );
    }
    assert_eq!(last["shared"].len(), N as usize + 2 * COALESCE_WIDTH);
}

/// **A replacement that does not stand for exactly its window's entities is refused**, and this is
/// the guard whose absence has no symptom.
///
/// Replacing N layers with one is not appending: the composition's coverage is adjusted by removing
/// the consumed layers and adding the replacement, so a replacement short of its inputs leaves the
/// column silently missing those entities' terms from every later `match` — and no cardinality
/// anywhere else moves. The merge's own guards cannot see it, presence being the only thing that
/// says what a layer stands for.
///
/// Two shapes, and both must refuse rather than publish: a window naming a layer this generation
/// does not hold (the plan and the process disagree about what the bundle is), and a replacement
/// whose presence is short of the union it replaces.
#[test]
fn a_coalesced_text_layer_that_does_not_cover_its_window_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().join("bundle");
    build_text_fixture(&root, dir.path());
    let engine = {
        let mut engine = Engine::open(
            &root,
            &dir.path().join("cache"),
            &dir.path().join("wal.log"),
            tessera_plugin::Passthrough::new(),
            config_uncapped(),
        )
        .expect("the engine opens against the text fixture");
        engine.start_write_executor(8).expect("the executor starts");
        engine.set_background_refresh_for_test(false);
        engine.set_merge_for_test(false);
        engine
    };
    flush_a_window(&engine, &root, 0);

    let prefix = root.join(current_prefix(&root));
    let extent = text_extents(&root)
        .into_iter()
        .find(|e| e.column == "prose")
        .expect("the coalesced extent");
    let columns = &engine.generation().filter_columns;
    let paths = |presence: &Path| tessera_engine::filter::TextExtentPaths {
        column: "prose".to_string(),
        dict_rel: extent.dict.clone(),
        dict: prefix.join(&extent.dict),
        postings: prefix.join(&extent.postings),
        presence: presence.to_path_buf(),
    };

    // A layer this generation does not hold.
    let stray = tessera_engine::filter::CoalescedTextWindow {
        consumed: vec!["attrs/prose/extents/never-dict.bin".to_string()],
        paths: paths(&prefix.join(&extent.presence)),
    };
    assert!(columns.with_coalesced(&[], &[stray], None, None).is_err());

    // And a replacement short of its window: the same dictionary and postings, a presence bitmap
    // with one entity removed.
    let mut short = croaring::Bitmap::deserialize::<croaring::Portable>(
        &std::fs::read(prefix.join(&extent.presence)).expect("the presence file reads"),
    );
    let dropped = short
        .minimum()
        .expect("the coalesced layer covers something");
    short.remove(dropped);
    let short_path = dir.path().join("short-presence.roaring");
    std::fs::write(&short_path, short.serialize::<croaring::Portable>()).unwrap();
    let window = tessera_engine::filter::CoalescedTextWindow {
        consumed: vec![extent.dict.clone()],
        paths: paths(&short_path),
    };
    let err = columns
        .with_coalesced(&[], &[window], None, None)
        .expect_err("a replacement short of its window is refused");
    assert!(format!("{err}").contains("present for"), "{err}");
}
