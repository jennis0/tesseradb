//! **One deletion, and every home it has to reach** — write-path §5.4's Rule F, asserted across
//! all of them at once instead of one at a time.
//!
//! An item's data does not live in one place. Records §3 gives every declared field exactly one of
//! three homes — the hot column in row space (`render`), the family's entity-space structure
//! (`index`), or the per-entity record blob (neither key set) — and beside those an item has a row
//! in row space, a bit in the render presence bitmap decision 0064 added, a binding in the
//! external-id sidecar, and the access-control postings that decide whether it is reachable at all.
//! The compaction fold rewrites all of them.
//!
//! Each of those rewrites is covered where it lives, and **nothing asserted that they agree**.
//! That is the gap this file closes: a home added to the corpus and forgotten by the fold leaves
//! every existing case passing while a deleted item's bytes stay in the bundle. [`Home`] is the
//! enumeration, in one place; its `files` is an exhaustive match, so a variant added there does not
//! compile until it has been given a file set, and each variant has its own block of assertions
//! below carrying its name.
//!
//! **Rule S and Rule F are both here and must not be conflated** (write-path §5.4 — conflating
//! them is fail-open and has been caught in review twice). A suppression retires nothing and
//! touches no artefact: the suppressed item is the survivor every home assertion checks against,
//! and it folds through intact in all nine. A deletion retires only at the fold that executes it:
//! before that fold the deleted item is hidden and every artefact byte it owns is still on disc,
//! which is what makes the assertions after the fold statements about the fold rather than about
//! nothing.

mod common;

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{BinaryArray, Float64Array, Int32Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_authz::{PostingRef, PostingsReader};
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::{Engine, EngineConfig, ViewportRequest};
use tessera_lifecycle::wal::ChangeOp;
use tessera_store::read::{open_bundle, ColumnsRef, ScalarSlice};
use tessera_types::{AttrLocalId, EntityId, TermId};

// ---------------------------------------------------------------------------------------------
// The fixture — a schema that puts one item in every home at once
// ---------------------------------------------------------------------------------------------

/// Small on purpose: every assertion below is exhaustive over the corpus, and the fold is the
/// expensive step. Nothing here is about scale.
const N: u64 = 64;

/// The item deleted. It carries a value in every home: a band, a score, the keyword no other item
/// carries, a blob row, an external id and both terms' postings.
const DELETED_SOURCE: u64 = 9;

/// The item **suppressed**, and therefore the one every home assertion checks the survivors
/// through. A suppression retires nothing (Rule S), so this item is hidden and simultaneously
/// present in all nine homes after the fold — which is what stops "gone from every home" being
/// satisfiable by a fold that simply emptied the artefacts.
const SUPPRESSED_SOURCE: u64 = 11;

/// The `idset` [`build_fixture_with_every_home`] mints under, which a `tessera_id`-addressed
/// change must carry (contracts §3.1).
const IDSET: u32 = 1;

/// The schema is the point of the fixture: one column per home.
///
/// - `band` is a `public` category, `render` **and** `index` — the only shape that owes membership
///   postings (decision 0063), so it is what gives the postings home an artefact.
/// - `score` is a rendered, indexed number **with absences**, which is the shape decision 0064's
///   presence bitmap exists for: it has a bit beside `columns.arrow` *and* a presence bitmap
///   beside its entity-space column, and the two are indexed differently (by row, by entity).
/// - `tag` is a keyword, so the column carries `u32` ordinals into a dictionary the fold rebuilds
///   from the survivors' values.
/// - `note` has neither key set and no vocabulary, so its only home is the record blob (records
///   §3).
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
index      = true
vocabulary = "band"

[[attribute]]
name   = "score"
type   = "i32"
render = true
index  = true

[[attribute]]
name  = "tag"
type  = "keyword"
index = true

# Blob-resident: a type with neither placement key, so its only home is the record blob. The
# type is `keyword` because `utf8` is retired as a declarable one — placement is orthogonal to
# type, and a blob row stores the value's bytes whatever family declared it.

[[attribute]]
name = "note"
type = "keyword"

# The only family with **two** homes at once: its prose is a blob row and its words are a token
# dictionary plus postings over it. Both have to be reached, and by different passes.

[[attribute]]
name     = "prose"
type     = "text"
index    = true
analyser = "unicode"
"#;

fn band_of(source: u64) -> &'static str {
    ["low", "mid", "high"][(source % 3) as usize]
}

fn band_code(source: u64) -> u8 {
    (source % 3) as u8 + 1
}

/// Every fourth item carries no score. Absence is what both presence bitmaps are for, and a
/// fixture without it exercises neither — a fold that dropped the render bitmap outright would
/// pass against a corpus where every row has a value.
fn score_of(source: u64) -> Option<i32> {
    if source.is_multiple_of(4) {
        None
    } else {
        Some(100 + source as i32)
    }
}

/// The deleted item's keyword is carried by **nothing else**, so the fold must drop the key from
/// the dictionary entirely (records §7); every fifth item carries no keyword at all, which is what
/// gives the column a presence bitmap to get wrong.
///
/// `m-sole-carrier` sorts *before* the shared keys rather than after them, deliberately: removing
/// the first key renumbers every surviving ordinal, so a fold that rebuilt the dictionary and left
/// the ordinals alone recolours every value instead of failing silently on none of them.
fn tag_of(source: u64) -> Option<String> {
    if source.is_multiple_of(5) {
        return None;
    }
    if source == DELETED_SOURCE {
        return Some("m-sole-carrier".to_string());
    }
    Some(format!("shared-tag-{}", source % 3))
}

/// The blob-resident value, unique per item so the byte walk over the folded blocks has something
/// to look for.
fn note_of(source: u64) -> String {
    format!("the-prose-of-{source:05}")
}

/// The indexed prose. Two words every item carries and one word that separates them, so the fold's
/// merge has both cases to get right: a term whose posting must lose one entity and keep the rest,
/// and a term whose only carrier is deleted and which must leave the dictionary altogether.
///
/// The deleted item's own word sorts **after** every survivor's, deliberately the opposite way
/// round from `tag_of`'s: a keyword's ordinals are stored per entity and a dropped first key
/// recolours them, where a text ordinal is a position nothing outside its own postings names — so
/// what this shape catches is different. A merge that emitted the term with an empty posting rather
/// than dropping it leaves a dictionary one key too long, and every posting after it is then read
/// against the wrong word.
fn prose_of(source: u64) -> String {
    if source == DELETED_SOURCE {
        return "shared prose solitonlattice".to_string();
    }
    format!("shared prose group{}", source % 3)
}

fn write_points(path: &Path, n: u64) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("band", DataType::Utf8, false),
        // Nullable, which is how absence reaches the build at all: the source's validity is what
        // decision 0064's two bitmaps are derived from.
        Field::new("score", DataType::Int32, true),
        Field::new("tag", DataType::Utf8, true),
        Field::new("note", DataType::Utf8, false),
        Field::new("prose", DataType::Utf8, false),
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
            Arc::new(Int32Array::from(
                ids.iter().map(|e| score_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| tag_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| note_of(*e)).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                ids.iter().map(|e| prose_of(*e)).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn build_fixture_with_every_home(out: &Path, tmp: &Path, n: u64) {
    let points = tmp.join("points.parquet");
    let pairs = tmp.join("pairs.parquet");
    write_points(&points, n);
    // `common`'s term assignment: every item carries `ALL_TERM`, every third `SUBSET_TERM` too.
    write_pairs_n(&pairs, n);
    let schema_path = tmp.join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &std::collections::HashMap::new())
        .map(|c| c.schema)
        .expect("the every-home fixture schema parses");
    let args = BuildArgs {
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
        idset: IDSET,
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
    };
    build(&args).expect("a build carrying all three homes succeeds");
}

fn engine_over(tmp: &Path, root: &Path, config: EngineConfig) -> Engine {
    let mut engine = Engine::open(
        root,
        &tmp.join("cache"),
        &tmp.join("wal.log"),
        tessera_plugin::Passthrough::new(),
        config,
    )
    .expect("the engine opens against the fixture");
    engine.start_write_executor(8).expect("the executor starts");
    engine.set_background_refresh_for_test(false);
    engine
}

/// Request a fold and block until it has published, asserting it was not discarded — the counter
/// is the deterministic wait, exactly as in `tests/fold.rs`.
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

/// Items a full-coverage principal is served across the whole extent.
fn visible(engine: &Engine, session: &tessera_engine::Session) -> u64 {
    engine
        .viewport(
            session,
            ViewportRequest::new("s0", 0, [0.0, 0.0, 1000.0, 1000.0], (N + 100) as usize),
        )
        .unwrap()
        .tiles
        .iter()
        .map(|tile| tile.visible)
        .sum()
}

/// The prefix `CURRENT` names — read rather than assumed, because a fold publishes into a new one
/// and a hard-coded `v00000` would read the *pre-fold* artefacts and pass without ever looking at
/// the fold's output.
fn current_prefix(root: &Path) -> String {
    let current: tessera_store::manifest::CurrentPointer =
        serde_json::from_slice(&std::fs::read(root.join("CURRENT")).expect("CURRENT is readable"))
            .expect("CURRENT parses");
    current.prefix
}

fn partition_dir(root: &Path) -> PathBuf {
    root.join(current_prefix(root)).join("partitions/default")
}

fn segment_dirs(root: &Path) -> Vec<PathBuf> {
    let bundle = open_bundle(root).expect("the bundle opens");
    let prefix = root.join(current_prefix(root));
    bundle.partitions["default"]
        .manifest
        .segments
        .iter()
        .map(|segment| {
            prefix
                .join("partitions/default/views")
                .join(&segment.view)
                .join("segments")
                .join(&segment.seg_id)
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The homes
// ---------------------------------------------------------------------------------------------

/// **Every place one item's data lives.**
///
/// A home added to the corpus must be added *here* and given its own block of assertions in
/// [`a_deletion_reaches_every_home`]. [`Home::files`] is an exhaustive match, so a variant added
/// without a file set does not compile; the assertion blocks carry the variant's name so the other
/// half is visible in review.
///
/// **A type rather than a comment**, because this set grows and every per-artefact case in the
/// suite passes on a fold that forgot the newest member of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Home {
    /// The item's row in row space, and the two directions between it and the entity.
    Row,
    /// A `render` column's value, in the hot row (records §3).
    RenderColumn,
    /// Which rows of a render column mean anything (decision 0064).
    RenderPresence,
    /// An `index` column's entity-space value column, with the presence bitmap beside it.
    ValueColumn,
    /// A public category's membership postings, rebuilt at the fold from the folded column.
    CategoryPostings,
    /// A keyword column's dictionary, where a key whose only carrier was deleted must vanish.
    KeywordDictionary,
    /// A text column's token index — the dictionary and the postings over it, which the fold
    /// rebuilds by merging every layer and subtracting the deleted set.
    TextIndex,
    /// The per-entity record blob: every field with neither key set (records §3).
    RecordBlob,
    /// The external-id binding, whose survival past a fold 409s a lawful re-ingest of the same key
    /// (decision 0047).
    ExternalIdSidecar,
    /// The access-control postings that decide whether the item is reachable at all.
    TermPostings,
}

impl Home {
    const ALL: [Home; 10] = [
        Home::Row,
        Home::RenderColumn,
        Home::RenderPresence,
        Home::ValueColumn,
        Home::CategoryPostings,
        Home::KeywordDictionary,
        Home::TextIndex,
        Home::RecordBlob,
        Home::ExternalIdSidecar,
        Home::TermPostings,
    ];

    /// The files this home's data is in, under the prefix `CURRENT` names **now**.
    ///
    /// Resolved on each call rather than cached, so the same function serves the pre-fold prefix
    /// and the folded one. The two homes that own a whole directory are listed by walking it, so
    /// an artefact added inside one is covered here without this match being touched; the rest are
    /// named by the readers' own path constants.
    fn files(self, root: &Path) -> Vec<PathBuf> {
        let partition = partition_dir(root);
        let attrs = partition.join("attrs");
        match self {
            Home::Row => {
                let view = partition.join("views/s0");
                vec![
                    view.join("permutation.bin"),
                    view.join(tessera_store::ROW_ENTITY_FILE),
                ]
            }
            Home::RenderColumn => segment_dirs(root)
                .iter()
                .map(|dir| dir.join("columns.arrow"))
                .collect(),
            Home::RenderPresence => segment_dirs(root)
                .iter()
                .map(|dir| tessera_store::render_presence::render_presence_path(dir, "score"))
                .collect(),
            // `band` is dense — every item carries one — so it owns no presence bitmap until the
            // fold takes a slot out of it. The two columns with absences own theirs from the build.
            Home::ValueColumn => vec![
                attrs.join("band").join(tessera_filter::VALUES_FILE),
                attrs.join("score").join(tessera_filter::VALUES_FILE),
                attrs.join("score").join(tessera_filter::PRESENCE_FILE),
                attrs.join("tag").join(tessera_filter::VALUES_FILE),
                attrs.join("tag").join(tessera_filter::PRESENCE_FILE),
            ],
            Home::CategoryPostings => vec![attrs.join("band/postings.arrow")],
            Home::KeywordDictionary => vec![attrs.join("tag").join(tessera_filter::DICT_FILE)],
            // Both halves, because they are one record: a posting is a position in *this*
            // dictionary, so a fold that rewrote one and carried the other forward answers every
            // `match` from the wrong words with no symptom.
            Home::TextIndex => vec![
                attrs.join("prose").join(tessera_filter::DICT_FILE),
                attrs.join("prose/postings.arrow"),
            ],
            Home::RecordBlob => files_under(&attrs.join("record")),
            Home::ExternalIdSidecar => files_under(&partition.join("entities")),
            Home::TermPostings => vec![partition.join("terms/postings.arrow")],
        }
    }
}

/// Every file directly under `dir`, sorted. Panics if the directory is missing, which is the right
/// answer for a home: a home with no artefact is a home this fixture has stopped exercising.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("a home's directory {} must exist: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry reads").path())
        .filter(|path| path.is_file())
        .collect();
    out.sort();
    assert!(!out.is_empty(), "{} holds no artefact", dir.display());
    out
}

/// Every home's files and their bytes, for the pre-fold "nothing was touched" comparison.
fn every_home_byte(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    for home in Home::ALL {
        for path in home.files(root) {
            let bytes = std::fs::read(&path).unwrap_or_else(|e| {
                panic!(
                    "{home:?} names {} but it does not read: {e}",
                    path.display()
                )
            });
            out.insert(path, bytes);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// One reader per home — where to look. What must hold is at the call sites.
// ---------------------------------------------------------------------------------------------

/// `Home::Row`: the row this entity resolves to, through the only legal entity → row path (I4).
fn row_of(root: &Path, entity: EntityId) -> Option<u32> {
    let bundle = open_bundle(root).expect("the bundle opens");
    bundle.partitions["default"].views["s0"]
        .row_space
        .row_of(entity)
        .map(|row| row.raw())
}

/// `Home::RenderColumn` and `Home::RenderPresence`: every live row's `tessera_id`, its two
/// rendered values, and whether the render presence bitmap calls its `score` meaningful.
///
/// Keyed by row within its segment, and returned with the segment's own bitmap, because that is
/// the numbering both artefacts are indexed by — and it is the numbering the fold rewrites.
fn rendered_rows(root: &Path) -> Vec<(u64, u8, i32, bool)> {
    let mut out = Vec::new();
    for dir in segment_dirs(root) {
        let columns = ColumnsRef::load(&dir.join("columns.arrow")).expect("a segment opens");
        let ids = columns.tessera_id();
        let Some(ScalarSlice::U8(band)) = columns.scalar("band") else {
            panic!("a segment must carry 'band' as u8");
        };
        let Some(ScalarSlice::I32(score)) = columns.scalar("score") else {
            panic!("a segment must carry 'score' as i32");
        };
        let presence = columns.presence("score");
        for row in 0..ids.len() {
            out.push((
                ids[row],
                band[row],
                score[row],
                presence.contains(row as u32),
            ));
        }
    }
    out
}

/// `Home::ValueColumn`: an indexed column's entity-space value for one entity, or `None` where it
/// holds none — read off the artefact through the reader the scan uses.
///
/// The answer is the raw code: a category's vocabulary code, a keyword's ordinal into its own
/// dictionary. A number's is not expressible this way (`Codes::at` answers `u32::MAX` for anything
/// that is not a category width), so `score` is read through [`scored_entities`] instead.
fn value_column_code(root: &Path, column: &str, entity: EntityId) -> Option<u32> {
    let dir = partition_dir(root).join("attrs").join(column);
    tessera_filter::ValueColumn::open_dir(&dir, tessera_filter::Access::Read)
        .expect("the column opens")
        .value_of(entity.raw() as u32)
        .map(|code| code.raw())
}

/// `Home::ValueColumn`, for the numeric column: the entities the column holds any value for.
fn scored_entities(root: &Path) -> croaring::Bitmap {
    let dir = partition_dir(root).join("attrs/score");
    tessera_filter::ValueColumn::open_dir(&dir, tessera_filter::Access::Read)
        .expect("the score column opens")
        .present()
}

/// `Home::CategoryPostings`: the entities `band` says carry `code`.
fn band_carriers(root: &Path, code: u8) -> croaring::Bitmap {
    let path = partition_dir(root).join("attrs/band/postings.arrow");
    tessera_filter::ColumnPostings::open_keyed(&path)
        .expect("the category postings open")
        .entities(AttrLocalId::new(u32::from(code)))
        .expect("the postings read")
}

/// `Home::KeywordDictionary`: every key the `tag` dictionary holds, in ordinal order.
fn keyword_dictionary(root: &Path) -> Vec<(u32, String)> {
    let dir = partition_dir(root).join("attrs/tag");
    let dict = tessera_filter::SortedDict::open_dir(&dir, tessera_filter::Access::Read)
        .expect("the keyword dictionary opens");
    let mut keys = Vec::new();
    dict.walk(|ordinal, key| keys.push((ordinal, key.to_string())))
        .expect("the dictionary walks");
    keys
}

/// `Home::TextIndex`: every term the `prose` index holds, with the entities its posting names.
///
/// Read through the two artefacts together — `key_of` for the word, the posting at the same
/// ordinal for its carriers — which is the pairing the whole family rests on and the one a fold
/// that rewrote only one half would break.
fn text_index(root: &Path) -> BTreeMap<String, croaring::Bitmap> {
    let dir = partition_dir(root).join("attrs/prose");
    let dict = tessera_filter::SortedDict::open_dir(&dir, tessera_filter::Access::Read)
        .expect("the token dictionary opens");
    let postings = tessera_filter::ColumnPostings::open(&dir.join("postings.arrow"), false)
        .expect("the token postings open");
    assert_eq!(
        dict.len(),
        postings.record_count(),
        "Home::TextIndex: the dictionary and the postings disagree about how many terms there are"
    );
    let mut out = BTreeMap::new();
    dict.walk(|ordinal, key| {
        out.insert(
            key.to_string(),
            postings
                .entities(AttrLocalId::new(ordinal))
                .expect("a posting reads"),
        );
    })
    .expect("the token dictionary walks");
    out
}

/// `Home::RecordBlob`: the blob's base, opened at the artefact — no mask, no overlay, no session.
fn record_blob(root: &Path) -> tessera_filter::RecordBlob {
    tessera_filter::RecordBlob::open_dir(
        &partition_dir(root).join("attrs/record"),
        tessera_filter::Access::Read,
    )
    .expect("the record blob opens")
}

/// `Home::ExternalIdSidecar`: source id → entity, decoded from **every** run the manifest names.
///
/// `common::source_to_new_map` reads only the first run. Asserting that a key is *gone* has to
/// look at all of them or it asserts nothing.
fn sidecar_bindings(root: &Path) -> BTreeMap<u64, u64> {
    let bundle = open_bundle(root).expect("the bundle opens");
    let prefix = root.join(current_prefix(root));
    let mut map = BTreeMap::new();
    for rel in &bundle.partitions["default"].manifest.external_id_runs {
        let file = File::open(prefix.join(rel)).expect("an external-id run opens");
        let reader = arrow::ipc::reader::FileReader::try_new(file, None).expect("the run reads");
        for batch in reader {
            let batch = batch.expect("a run batch reads");
            let ext = batch
                .column(0)
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("the key column is binary");
            let entity = batch
                .column(1)
                .as_any()
                .downcast_ref::<UInt32Array>()
                .expect("the entity column is u32");
            for i in 0..batch.num_rows() {
                map.insert(
                    u64::from_le_bytes(ext.value(i).try_into().unwrap()),
                    u64::from(entity.value(i)),
                );
            }
        }
    }
    map
}

/// `Home::TermPostings`: whether the base term index still names `entity` under `term`.
/// The term id `ALL_TERM`'s descriptor was interned at, read from the bundle's own dictionary.
///
/// **Resolved, not assumed.** `public` is reserved at term 0 by every build, so a descriptor's
/// ordinal is a fact about the corpus rather than a constant a test may spell — and a test that
/// spelled one would fail the day another reservation moved it, for a reason unrelated to what it
/// asserts.
fn all_term(root: &Path) -> TermId {
    let prefix = root.join(current_prefix(root));
    let bundle = open_bundle(root).expect("the bundle opens");
    let paths: Vec<PathBuf> = bundle.partitions["default"]
        .manifest
        .dict_extents
        .iter()
        .map(|extent| prefix.join(&extent.path))
        .collect();
    tessera_authz::Dict::load(&paths)
        .expect("the dictionary loads")
        .lookup(ALL_TERM.to_string().as_bytes())
        .expect("every item carries ALL_TERM")
}

fn term_names(root: &Path, term: TermId, entity: EntityId) -> bool {
    let path = partition_dir(root).join("terms/postings.arrow");
    let postings = PostingsReader::open(&path, false).expect("the term postings open");
    let entity = entity.raw() as u32;
    // Bound rather than returned as the tail expression, which would drop `postings` before the
    // `PostingRef` borrowing it.
    let named = match postings.posting(term).expect("the postings file reads") {
        None => false,
        Some(PostingRef::Roaring(view)) => view.contains(entity),
        Some(PostingRef::Array(bytes)) => bytes
            .chunks_exact(4)
            .any(|c| u32::from_le_bytes(c.try_into().unwrap()) == entity),
    };
    named
}

// ---------------------------------------------------------------------------------------------

/// **A deletion reaches every home, and a suppression reaches none of them.**
///
/// The shape is three phases. Before any change, the deleted item is asserted *present* in all
/// nine homes — without that the absences afterwards would be satisfied by a fixture that never
/// put it there. Then the change is accepted: the item is hidden immediately and not one artefact
/// byte moves, which is Rule F's first half and Rule S's whole of it. Then the fold, after which
/// each home is asserted individually, against the suppressed item as the survivor.
///
/// **What this kills that the per-home cases do not:** a home the fold forgets. Every existing
/// case tests one artefact, so a value column rewritten without its postings, a blob rewritten
/// without the sidecar, or a render bitmap carried across the renumbering unchanged all leave the
/// suite green while a deleted item's bytes stay in the bundle.
#[test]
fn a_deletion_reaches_every_home() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("bundle");
    build_fixture_with_every_home(&root, tmp.path(), N);
    let engine = engine_over(tmp.path(), &root, config_uncapped());

    let entity_of_source = source_to_new_map(&root, "v00000");
    let source_of_entity: BTreeMap<u64, u64> =
        entity_of_source.iter().map(|(s, e)| (*e, *s)).collect();
    let deleted = EntityId::new(entity_of_source[&DELETED_SOURCE]);
    let survivor = EntityId::new(entity_of_source[&SUPPRESSED_SOURCE]);

    // **Addressed the way a caller addresses a change.** The control plane takes exactly one of
    // `external_id` or `tessera_id` (contracts §3.1) and resolves both to an `EntityId` before the
    // deny lane sees anything, so the engine's change path takes only the entity — there is no
    // delete-by-external-id route below this line to drive. What is checkable here is that the two
    // resolutions name the same item, and the delete goes through the `tessera_id` one.
    let by_external = engine
        .resolve_external_id(&source_id_key(DELETED_SOURCE))
        .expect("the sidecar reads")
        .expect("the deleted item's external id resolves before the fold");
    let by_tessera = engine
        .resolve_tessera_ids(
            &[engine.tessera_id_of(deleted).expect("a wire identifier")],
            IDSET,
        )
        .expect("the idset is the deployment's")[0]
        .expect("the identifier names a live item");
    assert_eq!(by_external, deleted, "the external-id route names the item");
    assert_eq!(by_tessera, deleted, "and so does the tessera_id route");

    let baseline = {
        let session = engine.authorise(&full_coverage_credential()).unwrap();
        visible(&engine, &session)
    };
    assert_eq!(
        baseline, N,
        "a full-coverage principal starts with the corpus"
    );

    // ---- Phase 1: the item is in every home -------------------------------------------------
    //
    // Not ceremony. Every assertion after the fold is an absence, and an absence from a home the
    // fixture never wrote is free.
    assert!(row_of(&root, deleted).is_some(), "Home::Row");
    let deleted_wire = engine.tessera_id_of(deleted).unwrap().raw();
    assert!(
        rendered_rows(&root)
            .iter()
            .any(|(id, band, score, present)| *id == deleted_wire
                && *band == band_code(DELETED_SOURCE)
                && *score == score_of(DELETED_SOURCE).unwrap()
                && *present),
        "Home::RenderColumn / Home::RenderPresence"
    );
    assert_eq!(
        value_column_code(&root, "band", deleted),
        Some(u32::from(band_code(DELETED_SOURCE))),
        "Home::ValueColumn (category)"
    );
    assert!(
        scored_entities(&root).contains(deleted.raw() as u32),
        "Home::ValueColumn (number)"
    );
    assert!(
        value_column_code(&root, "tag", deleted).is_some(),
        "Home::ValueColumn (keyword ordinal)"
    );
    assert!(
        band_carriers(&root, band_code(DELETED_SOURCE)).contains(deleted.raw() as u32),
        "Home::CategoryPostings"
    );
    assert!(
        keyword_dictionary(&root)
            .iter()
            .any(|(_, key)| key == "m-sole-carrier"),
        "Home::KeywordDictionary"
    );
    let index = text_index(&root);
    assert!(
        index["solitonlattice"].contains(deleted.raw() as u32),
        "Home::TextIndex (the sole-carried word)"
    );
    assert!(
        index["shared"].contains(deleted.raw() as u32),
        "Home::TextIndex (a word the survivors carry too)"
    );
    assert!(
        record_blob(&root)
            .has_row(deleted.raw() as u32)
            .expect("a served blob holds its has-row bitmap"),
        "Home::RecordBlob"
    );
    assert_eq!(
        sidecar_bindings(&root).get(&DELETED_SOURCE),
        Some(&deleted.raw()),
        "Home::ExternalIdSidecar"
    );
    assert!(
        term_names(&root, all_term(&root), deleted),
        "Home::TermPostings"
    );

    // ---- Phase 2: accepted, hidden, and not one byte moved ----------------------------------
    //
    // Write-path §5.4, both rules, on the files. A suppression retires nothing while it stands
    // (Rule S); a deletion retires only at the fold that executes it (Rule F), so between
    // acceptance and that fold it is a mask over artefacts that still hold every byte. Giving
    // either any other retirement route is fail-open, and the byte comparison is what would catch
    // an eager rewrite trying to be helpful.
    let before = every_home_byte(&root);
    engine
        .accept_change(survivor, ChangeOp::Suppress)
        .expect("a suppression is accepted");
    engine
        .accept_change(by_tessera, ChangeOp::Delete)
        .expect("a deletion is accepted");
    assert_eq!(
        engine.overlay_depth(),
        2,
        "both entries stand in the overlay"
    );
    let hidden = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &hidden),
        baseline - 2,
        "both items are hidden the moment their change is accepted, whatever the artefacts hold"
    );
    assert_eq!(
        before,
        every_home_byte(&root),
        "an accepted change rewrote an artefact: Rule S touches nothing ever, and Rule F touches \
         nothing until the fold"
    );
    assert!(
        engine
            .resolve_external_id(&source_id_key(DELETED_SOURCE))
            .expect("the sidecar reads")
            .is_some(),
        "and the binding is still there to resolve, which is what the fold has to remove"
    );

    // ---- Phase 3: the fold executes it, everywhere -------------------------------------------
    fold(&engine);

    assert_eq!(
        engine.overlay_depth(),
        1,
        "Rule F retired the deletion at the fold that executed it; Rule S left the suppression \
         standing, and an entry retired by anything else re-exposes its item"
    );

    // The two entity ids as the entity-space artefacts index them — a bitmap slot and a value
    // column's addressing key, **never** a row: reaching a row from an entity is the permutation's
    // business alone (I4).
    let deleted_u32 = deleted.raw() as u32;
    let survivor_u32 = survivor.raw() as u32;

    // Home::Row — the row itself, and the two directions between it and the entity.
    assert!(
        row_of(&root, deleted).is_none(),
        "Home::Row: the deleted entity still resolves to a row"
    );
    assert!(
        row_of(&root, survivor).is_some(),
        "Home::Row: a suppressed entity kept its row, which is what a later unsuppress reveals"
    );

    // Home::RenderColumn — the hot column in row space. Asserted by identity rather than by row,
    // because the fold rewrites the whole permutation: a row-indexed check would pass on a fold
    // that carried the values forward unpermuted, every value against the wrong item.
    let rows = rendered_rows(&root);
    assert_eq!(
        rows.len() as u64,
        N - 1,
        "Home::RenderColumn: row space holds exactly the survivors"
    );
    assert!(
        !rows.iter().any(|(id, ..)| *id == deleted_wire),
        "Home::RenderColumn: the deleted item still has a rendered row"
    );
    let wire_of: BTreeMap<u64, u64> = entity_of_source
        .values()
        .map(|entity| {
            (
                engine
                    .tessera_id_of(EntityId::new(*entity))
                    .expect("a wire identifier")
                    .raw(),
                *entity,
            )
        })
        .collect();
    for (wire, band, score, present) in &rows {
        let source = source_of_entity[&wire_of[wire]];
        assert_eq!(
            *band,
            band_code(source),
            "Home::RenderColumn: source {source}'s band moved to another item's row"
        );
        // Home::RenderPresence — decision 0064's bitmap, and the assertion the fold's renumbering
        // is most likely to break. It is checked per *identity* over every surviving row, so a
        // bitmap carried across the fold unpermuted — which leaves the deleted row's index
        // describing whichever item slid down into it — fails here rather than showing an item
        // with no score as having one. **Measured on this fixture**: 20 of the 63 surviving rows
        // carry a different bit than their own pre-fold index does, so the carried-forward defect
        // fails here rather than passing on a corpus where the two happen to agree.
        assert_eq!(
            *present,
            score_of(source).is_some(),
            "Home::RenderPresence: source {source}'s presence bit belongs to another row"
        );
        if let Some(expected) = score_of(source) {
            assert_eq!(
                *score, expected,
                "Home::RenderColumn: source {source}'s score"
            );
        }
    }

    // Home::ValueColumn — the entity-space structures, one per indexed family.
    assert_eq!(
        value_column_code(&root, "band", deleted),
        None,
        "Home::ValueColumn: the deleted entity still carries a category code"
    );
    assert_eq!(
        value_column_code(&root, "band", survivor),
        Some(u32::from(band_code(SUPPRESSED_SOURCE))),
        "Home::ValueColumn: a suppressed entity's category value folds through intact"
    );
    let scored = scored_entities(&root);
    assert!(
        !scored.contains(deleted_u32),
        "Home::ValueColumn: the deleted entity still carries a number"
    );
    assert!(
        scored.contains(survivor_u32),
        "Home::ValueColumn: a suppressed entity's number folds through intact"
    );
    for (source, entity) in &entity_of_source {
        if *source == DELETED_SOURCE {
            continue;
        }
        assert_eq!(
            scored.contains(*entity as u32),
            score_of(*source).is_some(),
            "Home::ValueColumn: source {source}'s presence in the number column"
        );
    }

    // Home::CategoryPostings — rebuilt from the folded column, so a pass that rewrote the values
    // and carried the postings forward shows up here and nowhere else.
    assert!(
        !band_carriers(&root, band_code(DELETED_SOURCE)).contains(deleted_u32),
        "Home::CategoryPostings: a posting still names the deleted entity"
    );
    assert!(
        band_carriers(&root, band_code(SUPPRESSED_SOURCE)).contains(survivor_u32),
        "Home::CategoryPostings: a suppressed entity left its own postings"
    );

    // Home::KeywordDictionary — the deleted item was the sole carrier of its key, so the key
    // leaves the corpus (records §7), and every surviving key is renumbered against the rebuilt
    // dictionary. The ordinal round trip is what makes the renumbering checkable: a rebuild that
    // dropped the key and left the ordinals alone recolours every value below it.
    let dictionary = keyword_dictionary(&root);
    assert!(
        !dictionary.iter().any(|(_, key)| key == "m-sole-carrier"),
        "Home::KeywordDictionary: the deleted entity was the only carrier and its key survives: \
         {dictionary:?}"
    );
    let expected_keys: std::collections::BTreeSet<String> = entity_of_source
        .keys()
        .filter(|source| **source != DELETED_SOURCE)
        .filter_map(|source| tag_of(*source))
        .collect();
    assert_eq!(
        dictionary
            .iter()
            .map(|(_, key)| key.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        expected_keys,
        "Home::KeywordDictionary: the rebuilt dictionary is not the survivors' values"
    );
    for (source, entity) in &entity_of_source {
        if *source == DELETED_SOURCE {
            continue;
        }
        let ordinal = value_column_code(&root, "tag", EntityId::new(*entity));
        match tag_of(*source) {
            None => assert_eq!(
                ordinal, None,
                "Home::KeywordDictionary: source {source} carries no keyword and holds an ordinal"
            ),
            Some(key) => {
                let ordinal =
                    ordinal.unwrap_or_else(|| panic!("source {source} lost its keyword ordinal"));
                assert_eq!(
                    dictionary[ordinal as usize].1, key,
                    "Home::KeywordDictionary: source {source}'s ordinal resolves to the wrong key"
                );
            }
        }
    }

    // Home::TextIndex — the merge and the subtraction, and the two failures are different.
    //
    // A term the deleted item shared keeps its posting and loses exactly that entity; a term it
    // alone carried is not in the merged dictionary at all, so the word leaves the corpus with the
    // document that used it. Checked as a whole map rather than key by key: what a merge gets
    // wrong is the *pairing* — an emptied term left in place shifts every posting after it onto
    // the wrong word — and only comparing the full term set against the survivors' own prose
    // catches that.
    let index = text_index(&root);
    assert!(
        !index.contains_key("solitonlattice"),
        "Home::TextIndex: the deleted entity was the only carrier and its word survives: {:?}",
        index.keys().collect::<Vec<_>>()
    );
    let mut expected: BTreeMap<String, croaring::Bitmap> = BTreeMap::new();
    for (source, entity) in &entity_of_source {
        if *source == DELETED_SOURCE {
            continue;
        }
        for word in prose_of(*source).split(' ') {
            expected
                .entry(word.to_string())
                .or_default()
                .add(*entity as u32);
        }
    }
    assert_eq!(
        index.keys().collect::<Vec<_>>(),
        expected.keys().collect::<Vec<_>>(),
        "Home::TextIndex: the merged dictionary is not the survivors' vocabulary"
    );
    for (word, carriers) in &expected {
        assert_eq!(
            &index[word], carriers,
            "Home::TextIndex: '{word}' names the wrong entities after the fold"
        );
    }
    assert!(
        index["shared"].contains(survivor_u32),
        "Home::TextIndex: a suppressed entity's words folded through intact"
    );

    // Home::RecordBlob — byte-absent, against the artefact's own bytes.
    //
    // The walk is the byte argument: `for_each_row` refuses any block whose rows do not tile it
    // exactly, so once every decoded row is a surviving entity's expected prose, every block byte
    // is accounted for and none of them is the deleted row's. It reads the file, applies no mask
    // and consults no overlay, so it cannot be a reader filtering the row out. The raw scan below
    // it is the cheap second guard — vacuous on its own, since the blocks are zstd frames, but it
    // catches a block that reached the artefact uncompressed. The decompressed grep is the unit
    // half's (`tessera-filter-write`'s `a_blanked_rows_bytes_are_not_in_the_folded_blob`).
    let blob = record_blob(&root);
    assert!(
        !blob
            .has_row(deleted_u32)
            .expect("a served blob holds its has-row bitmap"),
        "Home::RecordBlob: the deleted entity is still in the has-row bitmap"
    );
    assert_eq!(
        blob.fields_of(deleted_u32).expect("a clean read"),
        None,
        "Home::RecordBlob: the deleted entity still has a row"
    );
    let mut walked = 0u64;
    blob.for_each_row(&mut |entity, fields| {
        assert_ne!(
            entity, deleted_u32,
            "Home::RecordBlob: the deleted entity has a row in the folded blob"
        );
        let source = source_of_entity[&u64::from(entity)];
        assert_eq!(
            fields.len(),
            2,
            "Home::RecordBlob: source {source}'s row is not its two blob-resident fields"
        );
        assert_eq!(
            fields[0].value,
            tessera_filter::RecordValue::Utf8(note_of(source)),
            "Home::RecordBlob: source {source}'s note"
        );
        // The text column's **other** home. Its words are in the token index above; its bytes are
        // here, and a fold that reached one and not the other leaves the deleted item's prose
        // readable through a drill-down while `match` no longer names it.
        assert_eq!(
            fields[1].value,
            tessera_filter::RecordValue::Utf8(prose_of(source)),
            "Home::RecordBlob: source {source}'s prose"
        );
        walked += 1;
        Ok(())
    })
    .expect("the folded blob walks clean");
    assert_eq!(
        walked,
        N - 1,
        "Home::RecordBlob: the walk did not cover the survivors"
    );
    let blocks = std::fs::read(
        partition_dir(&root)
            .join("attrs/record")
            .join(tessera_filter::RECORD_BLOCKS_FILE),
    )
    .expect("the folded blocks read");
    let needle = note_of(DELETED_SOURCE).into_bytes();
    assert!(
        !blocks.windows(needle.len()).any(|w| w == needle),
        "Home::RecordBlob: the deleted prose is in the folded file verbatim"
    );

    // Home::ExternalIdSidecar — the key resolves to nothing, through the live engine and in the
    // run the fold wrote. Leaving it standing 409s a lawful re-ingest of that key for ever
    // (decision 0047: edit is delete plus re-ingest).
    assert_eq!(
        engine
            .resolve_external_id(&source_id_key(DELETED_SOURCE))
            .expect("the sidecar reads"),
        None,
        "Home::ExternalIdSidecar: the deleted item's key still resolves"
    );
    let bindings = sidecar_bindings(&root);
    assert!(
        !bindings.contains_key(&DELETED_SOURCE),
        "Home::ExternalIdSidecar: the folded run still binds the deleted key"
    );
    assert_eq!(
        bindings.get(&SUPPRESSED_SOURCE),
        Some(&survivor.raw()),
        "Home::ExternalIdSidecar: a suppressed item's binding folds through intact"
    );
    assert_eq!(
        bindings.len() as u64,
        N - 1,
        "Home::ExternalIdSidecar: the fold dropped more than the executed key"
    );

    // Home::TermPostings — the access-control half. Both halves or neither: a row dropped with the
    // postings left standing survives Rule F's retirement and is served to every authorised
    // principal afterwards.
    assert!(
        !term_names(&root, all_term(&root), deleted),
        "Home::TermPostings: the deleted entity is still named by a term"
    );
    assert!(
        term_names(&root, all_term(&root), survivor),
        "Home::TermPostings: a suppressed entity left the term index, so the sweep took more than \
         the executed set"
    );

    // And the item is still hidden and still revealable, which is the whole of what the two rules
    // promise about these two items after the fold.
    let after = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(visible(&engine, &after), N - 2);
    engine
        .accept_change(survivor, ChangeOp::Unsuppress)
        .expect("an unsuppress is accepted");
    let revealed = engine.authorise(&full_coverage_credential()).unwrap();
    assert_eq!(
        visible(&engine, &revealed),
        N - 1,
        "the suppressed item folded through every home, so there is something to reveal"
    );
}
