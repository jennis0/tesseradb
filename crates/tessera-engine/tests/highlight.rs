//! **The `highlight` field: a second expression over the same candidate, three answers, and a
//! draw that does not move** (`highlight-and-hierarchy.md` §2).
//!
//! The property the whole design turns on is negative and is asserted first: **the served set is
//! identical with and without a highlight**. If it were not, a highlight would be a filter with a
//! different name — the map would resample as a viewer lit a cluster, and the point they were
//! looking at would move.
//!
//! What else is asserted: `highlighted = matched` on a request carrying no highlight and
//! `highlighted ≤ matched ≤ visible` on one that does; the per-tile count equals the `matched`
//! count the same request would report with `all_of[filters, highlight]` in `filters` instead,
//! which is C32's whole argument; the per-point bit is present exactly when the request carried a
//! highlight and agrees with the per-tile counts; **a highlight never takes the whole-view
//! projection**, checked against the engine's own route counters; and `point_rows = "highlight"`
//! serves the same rows in the same per-tile split.
//!
//! The oracle is the fixture's own generator throughout — the source values a request's clauses
//! are computed from directly — so an implementation that stored the wrong thing and read it back
//! consistently fails here.

mod common;

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use arrow::array::{Float64Array, Int32Array, StringArray, UInt32Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema as ArrowSchema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use common::*;
use tessera_build::config::Config;
use tessera_build::{build, BuildArgs};
use tessera_engine::filter::{Endpoint, FilterExpr, FilterOperand, Scalar};
use tessera_types::AttrLocalId;
use tessera_engine::{Engine, PointRows, ViewportOut, ViewportRequest};

/// Enough items that a depth-2 request splits into several non-empty tiles and the cap clause has
/// something to cap, and small enough that the fixture builds in a moment.
const N: u64 = 600;
const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];

/// Two public categories and one number. `topic` is **`index` only**, which is the shape that
/// routes entity space whatever the request's span — so a clause over it is the one that exercises
/// the crossing this design forces per-tile. `archive` and `score` are rendered and indexed, which
/// affords both routes, and that is what makes the two positions of a clause worth comparing.
const SCHEMA_TOML: &str = r#"
[[vocabulary]]
name       = "archive"
width      = "u8"
value_set  = "closed"
visibility = "public"
  [vocabulary.values]
  xx = 11
  yy = 22
  zz = 33

[[attribute]]
name       = "archive"
type       = "category"
render     = true
index      = true
vocabulary = "archive"

[[attribute]]
name       = "topic"
type       = "category"
render     = false
index      = true
vocabulary = "archive"

[[attribute]]
name     = "score"
type     = "i32"
render   = true
index    = true
"#;

/// **Decorrelated from the permission model**, which grants on `e % 3`: an archive keyed on the
/// same residue would make every "xx" item exactly what the narrow principal sees, and every
/// cross-principal assertion would pass while proving nothing.
fn archive_of(e: u64) -> &'static str {
    ["xx", "yy", "zz"][(e / 2 % 3) as usize]
}

fn score_of(e: u64) -> i32 {
    (e as i32 * 7 % 101) - 50
}

fn subset_sees(e: u64) -> bool {
    terms_of(e).contains(&SUBSET_TERM)
}

fn write_points(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("x", DataType::Float64, false),
        Field::new("y", DataType::Float64, false),
        Field::new("archive", DataType::Utf8, true),
        Field::new("topic", DataType::Utf8, true),
        Field::new("score", DataType::Int32, false),
    ]));
    let ids: Vec<u64> = (0..N).collect();
    let xs: Vec<f64> = ids.iter().map(|e| ((e * 37) % 1000) as f64).collect();
    let ys: Vec<f64> = ids.iter().map(|e| ((e * 53) % 1000) as f64).collect();
    let archives: Vec<Option<String>> = ids
        .iter()
        .map(|&e| Some(archive_of(e).to_string()))
        .collect();
    let scores: Vec<i32> = ids.iter().map(|&e| score_of(e)).collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(ids)),
            Arc::new(Float64Array::from(xs)),
            Arc::new(Float64Array::from(ys)),
            Arc::new(StringArray::from(archives.clone())),
            Arc::new(StringArray::from(archives)),
            Arc::new(Int32Array::from(scores)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

fn write_pairs(path: &Path) {
    let schema = Arc::new(ArrowSchema::new(vec![
        Field::new("entity_id", DataType::UInt64, false),
        Field::new("term_id", DataType::UInt32, false),
    ]));
    let mut entities: Vec<u64> = Vec::new();
    let mut terms: Vec<u32> = Vec::new();
    for e in 0..N {
        for t in terms_of(e) {
            entities.push(e);
            terms.push(t as u32);
        }
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from(entities)),
            Arc::new(UInt32Array::from(terms)),
        ],
    )
    .unwrap();
    let mut w = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    engine: Engine,
    /// The vocabulary's key-to-code bindings, read from the built manifest rather than from the
    /// declaration: a category leaf names a code, and reading the declaration back would be the
    /// test agreeing with itself about what the build did.
    codes: HashMap<String, u32>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.parquet");
    let pairs = dir.path().join("pairs.parquet");
    write_points(&points);
    write_pairs(&pairs);
    let bundle = dir.path().join("bundle");
    let schema_path = dir.path().join("schema.toml");
    std::fs::write(&schema_path, SCHEMA_TOML).unwrap();
    let schema = Config::parse(&schema_path, &HashMap::new()).unwrap().schema;
    build(&BuildArgs {
        arena_order: Default::default(),
        views: vec![tessera_build::ViewArgs {
            visibility: None,
            view_id: "s0".to_string(),
            projection: tessera_spatial::Projection::None,
            extent: extent(),
            points: points.clone(),
            point_fields: Default::default(),
            select: None,
            access: tessera_build::config::AccessInput::relation(pairs.clone()),
        }],
        anchor: 0,
        groups: Vec::new(),
        scoped_attributes: Vec::new(),
        attribute_sources: tessera_build::config::AttributeSource::over(points.clone(), &schema),
        out: bundle.clone(),
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
    .expect("the fixture builds");
    let engine = open_engine_uncapped(
        &bundle,
        &dir.path().join("cache"),
        &dir.path().join("wal.log"),
    );
    let opened = tessera_store::read::open_bundle(&bundle).expect("the fixture opens");
    let codes = opened
        .manifest
        .vocabularies
        .iter()
        .find(|v| v.name == "archive")
        .expect("the manifest records the vocabulary")
        .values
        .iter()
        .map(|v| (v.key.clone(), v.code))
        .collect();
    Fixture {
        _dir: dir,
        engine,
        codes,
    }
}

/// A category leaf over `column`, by the built vocabulary's own code.
fn category_is(fx: &Fixture, column: &str, key: &str) -> FilterExpr {
    FilterExpr::Leaf {
        column: column.into(),
        operand: FilterOperand::Equals(AttrLocalId::new(fx.codes[key])),
    }
}

/// A numeric range — the both-routes column, so a clause over it may route either way.
fn score_below(bound: i32) -> FilterExpr {
    FilterExpr::Leaf {
        column: "score".into(),
        operand: FilterOperand::Range {
            lo: None,
            hi: Some(Endpoint {
                value: Scalar::Int(bound as i128),
                inclusive: false,
            }),
        },
    }
}

struct Ask {
    filter: Option<FilterExpr>,
    highlight: Option<FilterExpr>,
    point_rows: PointRows,
}

impl Ask {
    fn new() -> Self {
        Ask {
            filter: None,
            highlight: None,
            point_rows: PointRows::Full,
        }
    }
    fn filter(mut self, expr: FilterExpr) -> Self {
        self.filter = Some(expr);
        self
    }
    fn highlight(mut self, expr: FilterExpr) -> Self {
        self.highlight = Some(expr);
        self
    }
    fn point_rows(mut self, rows: PointRows) -> Self {
        self.point_rows = rows;
        self
    }
}

fn viewport(
    engine: &Engine,
    credential: &[u8],
    zoom: u8,
    bbox: [f64; 4],
    ask: Ask,
) -> ViewportOut {
    let session = engine.authorise(credential).unwrap();
    let mut request =
        ViewportRequest::new("s0", zoom, bbox, N as usize).point_rows(ask.point_rows);
    if let Some(expr) = ask.filter {
        request = request.filter(expr);
    }
    if let Some(expr) = ask.highlight {
        request = request.highlight(expr);
    }
    engine
        .viewport(&session, request)
        .expect("a viewport answers")
}

/// Tile identity without the highlight column: `(tile, visible, matched, served)`.
fn draw(out: &ViewportOut) -> Vec<(u64, u64, u64, u64)> {
    out.tiles
        .iter()
        .map(|t| (t.tile, t.visible, t.matched, t.served))
        .collect()
}

fn highlighted_total(out: &ViewportOut) -> u64 {
    out.tiles.iter().map(|t| t.highlighted).sum()
}

fn matched_total(out: &ViewportOut) -> u64 {
    out.tiles.iter().map(|t| t.matched).sum()
}

/// The viewports the sweeps run over: the whole map at three depths, and two boxes that are
/// genuinely different slices of row space rather than three spellings of one.
const CELLS: [(u8, [f64; 4]); 5] = [
    (0, WHOLE_MAP),
    (1, WHOLE_MAP),
    (2, WHOLE_MAP),
    (2, [0.0, 0.0, 500.0, 500.0]),
    (3, [250.0, 250.0, 760.0, 760.0]),
];

/// **The served set is identical with and without a highlight** — the property that makes this a
/// highlight rather than a filter.
///
/// The cap clause, the density sampling and `served` all run over the `filters` candidate, so the
/// tile split, the point set and their order must be byte-identical between a request carrying a
/// highlight and the same request without one. Swept over five viewports, two principals, an
/// unfiltered and a filtered request, and two highlights of different selectivity.
///
/// Mutations this kills: folding the highlight into the mask's `filter` field, or letting
/// `rows_in_range` — which selection draws from — consult it. Either would narrow the draw, and
/// every count in the response would still be internally consistent.
#[test]
fn the_served_set_is_identical_with_and_without_a_highlight() {
    let fx = fixture();
    for credential in [full_coverage_credential(), subset_credential()] {
        for (zoom, bbox) in CELLS {
            for filter in [None, Some(score_below(0))] {
                let plain = viewport(
                    &fx.engine,
                    &credential,
                    zoom,
                    bbox,
                    match &filter {
                        None => Ask::new(),
                        Some(f) => Ask::new().filter(f.clone()),
                    },
                );
                for highlight in [category_is(&fx, "archive", "xx"), score_below(-40), FilterExpr::Leaf {
                        column: "archive".into(),
                        operand: FilterOperand::Equals(
                            tessera_engine::filter::UNRESOLVABLE_VALUE,
                        ),
                    }] {
                    let lit = viewport(
                        &fx.engine,
                        &credential,
                        zoom,
                        bbox,
                        match &filter {
                            None => Ask::new().highlight(highlight.clone()),
                            Some(f) => Ask::new().filter(f.clone()).highlight(highlight.clone()),
                        },
                    );
                    let what = format!("zoom {zoom}, bbox {bbox:?}, filter {}", filter.is_some());
                    assert_eq!(draw(&lit), draw(&plain), "{what}: the tile split moved");
                    assert_eq!(
                        lit.points.tessera_ids, plain.points.tessera_ids,
                        "{what}: the point set moved"
                    );
                    assert_eq!(lit.points.codes, plain.points.codes, "{what}: positions moved");
                    assert_eq!(
                        lit.points.scalars, plain.points.scalars,
                        "{what}: the render columns moved"
                    );
                }
            }
        }
    }
}

/// **`highlighted = matched` with no highlight, and `highlighted ≤ matched ≤ visible` with one.**
///
/// The first is why the column is always present rather than optional: an absent highlight is the
/// identity for this quantity, so a response without one says the same thing a missing column
/// would. The second holds by construction — each count is over a subset of the last — and is what
/// makes the wash comparable with the number beside it.
#[test]
fn the_three_counts_nest_and_an_absent_highlight_is_the_identity() {
    let fx = fixture();
    for credential in [full_coverage_credential(), subset_credential()] {
        for (zoom, bbox) in CELLS {
            let plain = viewport(&fx.engine, &credential, zoom, bbox, Ask::new());
            for tile in &plain.tiles {
                assert_eq!(
                    tile.highlighted, tile.matched,
                    "an absent highlight is the identity, tile {}",
                    tile.tile
                );
            }
            let lit = viewport(
                &fx.engine,
                &credential,
                zoom,
                bbox,
                Ask::new()
                    .filter(score_below(20))
                    .highlight(category_is(&fx, "archive", "yy")),
            );
            for tile in &lit.tiles {
                assert!(
                    tile.highlighted <= tile.matched && tile.matched <= tile.visible,
                    "highlighted {} ≤ matched {} ≤ visible {} at tile {}",
                    tile.highlighted,
                    tile.matched,
                    tile.visible,
                    tile.tile
                );
            }
            assert!(
                highlighted_total(&lit) > 0 && highlighted_total(&lit) < matched_total(&lit),
                "a real subset, not everything and not nothing"
            );
        }
    }
}

/// **The per-tile `highlighted` count is the `matched` count of `all_of[filters, highlight]`** —
/// C32's whole argument, checked rather than asserted in prose.
///
/// A highlight discloses what one request carrying the conjunction in `filters` would have
/// disclosed, differently arranged. So the two requests must agree tile for tile; if they did not,
/// the highlight would be answering some other question, and the register row would be describing
/// a quantity nobody serves.
#[test]
fn the_highlighted_count_is_the_conjunctions_matched_count() {
    let fx = fixture();
    for credential in [full_coverage_credential(), subset_credential()] {
        for (zoom, bbox) in CELLS {
            for (filter, highlight) in [
                (score_below(20), category_is(&fx, "archive", "yy")),
                (category_is(&fx, "archive", "zz"), score_below(-10)),
                (score_below(50), score_below(-30)),
            ] {
                let lit = viewport(
                    &fx.engine,
                    &credential,
                    zoom,
                    bbox,
                    Ask::new()
                        .filter(filter.clone())
                        .highlight(highlight.clone()),
                );
                let conjoined = viewport(
                    &fx.engine,
                    &credential,
                    zoom,
                    bbox,
                    Ask::new().filter(FilterExpr::AllOf(vec![filter, highlight])),
                );
                let lit_counts: Vec<(u64, u64)> =
                    lit.tiles.iter().map(|t| (t.tile, t.highlighted)).collect();
                let conjoined_counts: Vec<(u64, u64)> =
                    conjoined.tiles.iter().map(|t| (t.tile, t.matched)).collect();
                // The two responses hold the same tiles: `visible` decides which tiles appear and
                // neither expression moves it.
                assert_eq!(lit_counts, conjoined_counts, "zoom {zoom}, bbox {bbox:?}");
            }
        }
    }
}

/// **A highlight never takes the whole-view projection** (§2.1).
///
/// All three of a highlight's answers are inside the request's own tiles, so the projecting
/// crossing — which scales with what matched corpus-wide rather than with what is on screen — is
/// work paid for nothing: ~216 ms at a 10⁷-entity verdict against ~18 ms for the walk. The engine
/// counts the two routes, so this is checkable directly rather than by timing.
///
/// The filter beside it is deliberately one the measured rule *would* project — a small viewport
/// against a broad verdict — so the counter moving at all proves the route is chosen per
/// expression and not per request.
#[test]
fn a_highlight_never_projects_the_whole_view() {
    let fx = fixture();
    let session = fx.engine.authorise(&full_coverage_credential()).unwrap();
    let ask = |filter: Option<FilterExpr>, highlight: Option<FilterExpr>| {
        let mut request = ViewportRequest::new("s0", 3, [250.0, 250.0, 760.0, 760.0], N as usize);
        if let Some(expr) = filter {
            request = request.filter(expr);
        }
        if let Some(expr) = highlight {
            request = request.highlight(expr);
        }
        fx.engine.viewport(&session, request).unwrap();
    };
    // A baseline, so the counters below are read as differences rather than as totals.
    let (projected_before, per_tile_before) = fx.engine.filter_crossing_routes();
    ask(None, Some(category_is(&fx, "topic", "xx")));
    ask(
        None,
        Some(FilterExpr::AnyOf(vec![
            category_is(&fx, "topic", "xx"),
            category_is(&fx, "topic", "yy"),
            category_is(&fx, "topic", "zz"),
        ])),
    );
    let (projected_after, per_tile_after) = fx.engine.filter_crossing_routes();
    assert_eq!(
        projected_after, projected_before,
        "a highlight took the whole-view projection"
    );
    assert!(
        per_tile_after > per_tile_before,
        "and it crossed at all: {per_tile_before} → {per_tile_after}"
    );
}

/// **The per-point bit is present exactly when the request carried a highlight**, and over an
/// uncapped engine — where every match is served — its total is the tiles' own.
///
/// The absence is the assertion that matters: an all-false column would answer a question nobody
/// asked, and a client would have no way to tell it from a highlight that matched nothing.
#[test]
fn the_point_bit_is_present_only_with_a_highlight_and_agrees_with_the_counts() {
    let fx = fixture();
    for credential in [full_coverage_credential(), subset_credential()] {
        let plain = viewport(&fx.engine, &credential, 2, WHOLE_MAP, Ask::new());
        assert!(
            plain.points.highlighted.is_none(),
            "no highlight, no column — not a column of falses"
        );
        let lit = viewport(
            &fx.engine,
            &credential,
            2,
            WHOLE_MAP,
            Ask::new().highlight(category_is(&fx, "archive", "yy")),
        );
        let bits = lit
            .points
            .highlighted
            .as_ref()
            .expect("a highlight carries the column");
        assert_eq!(bits.len(), lit.points.len(), "one bit per served point");
        // The engine is uncapped and θ saturates under a filter, so at this budget every match is
        // served and the two channels count the same set.
        assert_eq!(
            bits.iter().filter(|&&b| b).count() as u64,
            highlighted_total(&lit),
            "the bits and the tile counts are the same answer"
        );
        // And the oracle: the fixture's own values, masked by this principal's grant.
        let broad = credential == full_coverage_credential();
        let want = (0..N)
            .filter(|&e| (broad || subset_sees(e)) && archive_of(e) == "yy")
            .count() as u64;
        assert_eq!(highlighted_total(&lit), want, "against the generator");
    }
}

/// **`point_rows = "highlight"` is a column projection: the same rows, the same split, fewer
/// columns.**
///
/// That sentence is the contract that makes the projection disclose nothing — it is a column
/// subset of what the same caller's identical request would have been served, because the served
/// set does not depend on the highlight at all. Without a highlight there is nothing to project
/// to, and the request answers as `"full"` does rather than serving a column of nulls.
#[test]
fn the_highlight_projection_serves_the_same_rows_in_the_same_split() {
    let fx = fixture();
    for credential in [full_coverage_credential(), subset_credential()] {
        for (zoom, bbox) in CELLS {
            let full = viewport(
                &fx.engine,
                &credential,
                zoom,
                bbox,
                Ask::new()
                    .filter(score_below(30))
                    .highlight(category_is(&fx, "archive", "xx")),
            );
            let projected = viewport(
                &fx.engine,
                &credential,
                zoom,
                bbox,
                Ask::new()
                    .filter(score_below(30))
                    .highlight(category_is(&fx, "archive", "xx"))
                    .point_rows(PointRows::Highlight),
            );
            assert_eq!(draw(&projected), draw(&full), "the tile split moved");
            assert_eq!(
                projected.points.tessera_ids, full.points.tessera_ids,
                "the row set moved"
            );
            assert_eq!(
                projected.points.highlighted, full.points.highlighted,
                "the bits moved"
            );
            assert!(
                projected.points.scalars.is_empty() && projected.scalar_names.is_empty(),
                "the render columns are not gathered under the projection"
            );
        }
        // No highlight to project to: the projection answers as `"full"` does.
        let bare = viewport(
            &fx.engine,
            &credential,
            2,
            WHOLE_MAP,
            Ask::new().point_rows(PointRows::Highlight),
        );
        let plain = viewport(&fx.engine, &credential, 2, WHOLE_MAP, Ask::new());
        assert_eq!(bare, plain, "without a highlight the projection is the full answer");
    }
}

/// **Every per-tile prefix stays sound with the new columns.**
///
/// §7.2's nesting argument depends on the served set being a *prefix* — ascending by `tessera_id`
/// within each tile, so a client truncating to its own budget keeps a valid smaller selection —
/// and the `highlighted` column has to travel with it. What could go wrong is not the order but
/// the alignment: a bit column gathered from a different order, or built once per response rather
/// than per tile, would leave a truncated prefix carrying another point's answer with nothing
/// saying so.
///
/// So this walks the response's tiles through `served`, checks each group ascends, and checks that
/// every prefix of every group carries exactly the bits the same points carry in the untruncated
/// answer — including under the `"highlight"` projection, where the bits are all the client gets.
#[test]
fn every_per_tile_prefix_carries_its_own_points_bits() {
    let fx = fixture();
    for credential in [full_coverage_credential(), subset_credential()] {
        for (zoom, bbox) in CELLS {
            let out = viewport(
                &fx.engine,
                &credential,
                zoom,
                bbox,
                Ask::new()
                    .filter(score_below(30))
                    .highlight(category_is(&fx, "archive", "xx")),
            );
            let bits = out.points.highlighted.as_ref().expect("a highlight");
            let projected = viewport(
                &fx.engine,
                &credential,
                zoom,
                bbox,
                Ask::new()
                    .filter(score_below(30))
                    .highlight(category_is(&fx, "archive", "xx"))
                    .point_rows(PointRows::Highlight),
            );
            let projected_bits = projected.points.highlighted.as_ref().expect("a highlight");
            let mut at = 0usize;
            for tile in &out.tiles {
                let end = at + tile.served as usize;
                let ids = &out.points.tessera_ids[at..end];
                assert!(
                    ids.windows(2).all(|w| w[0] < w[1]),
                    "tile {} is not ascending by tessera_id, so a prefix is not a selection",
                    tile.tile
                );
                for cut in 0..=ids.len() {
                    assert_eq!(
                        &bits[at..at + cut],
                        &projected_bits[at..at + cut],
                        "tile {}: a {cut}-point prefix disagrees between the two projections",
                        tile.tile
                    );
                }
                at = end;
            }
            assert_eq!(at, out.points.len(), "the tiles' `served` covers every point");
        }
    }
}
