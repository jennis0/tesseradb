//! **A boundary is the points inside it, exactly, resolved when a segment is published and never
//! stale.**
//!
//! A `membership = "spatial"` layer stores no membership at all. Each artifact declares a shape;
//! its members are the rows whose stored position is inside that shape — closed on every side for
//! a box, even-odd with an edge inside for a polygon (`polygon-membership.md` §4.1) — resolved
//! against each segment when the segment is published (§6.3) and joined per generation.
//!
//! **The oracle is the corpus generator and the shape's own direct test**: every count asserted
//! below is computed by quantising the generator's points to the grid — the same `fixed32` the
//! build applies — testing each against the canonical shape's direct predicate, and masking by
//! the principal's grant. The engine reaches the same answer through the descent, the per-segment
//! resolution, the row bases and the layout machinery, none of which the oracle touches; a defect
//! in the bookkeeping — a segment resolved twice or not at all, a row-base slip, a form built
//! under one generation and read under another — shows up as a disagreement.
//!
//! **The boxes are the fixture's**, not the generator's: `tessera-corpus`'s boundary arm carries a
//! roster of authored tile prefixes and no geometry. So this file builds a box per chosen tile —
//! the middle half of it — and two polygons over the map, and uses the generator only for the
//! points, the access relation and the visibility oracle.
//!
//! What is checked, in order: the counts against the oracle over several principals and viewports,
//! for a box layer and a polygon layer; that a point ingested inside a shape counts on the next
//! request with nothing rebuilt; that a suppressed member leaves the count and a suppressed
//! artifact leaves the map; that a fold leaves every answer where it was; that an absolute
//! criterion fires on such a layer exactly as it does on a stored one; and that the shapes survive
//! a restart.

mod common;

use std::collections::BTreeMap;

use common::*;
use tessera_corpus::{Corpus, Grant};
use tessera_engine::viewport::ViewportRequest;
use tessera_engine::Engine;
use tessera_lifecycle::command::UnallocatedRow;
use tessera_lifecycle::wal::ChangeOp;
use tessera_spatial::shape::Space;
use tessera_types::EntityId;

const N: u64 = 3_000;
const SEED: u64 = 0x5EED;
/// The depth of the tiles the fixture draws its boxes over — a fixture choice, not a membership
/// one: every shape is exact whatever tile it was drawn from.
const DEPTH: u8 = 3;
const LAYER: &str = "regions/boxes";
const POLYGONS: &str = "regions/polygons";
/// The polygon layer's two shapes, in extent coordinates: a diamond over the middle of the map and
/// a square with a square hole in the north-west, so the oracle exercises holes and edges alike.
const DIAMOND: &str = "POLYGON ((500 100, 900 500, 500 900, 100 500, 500 100))";
const FRAME: &str =
    "POLYGON ((50 50, 350 50, 350 350, 50 350, 50 50), (150 150, 250 150, 250 250, 150 250, 150 150))";
const WHOLE_MAP: [f64; 4] = [0.0, 0.0, 1000.0, 1000.0];
/// A polygon smaller than one depth-16 cell, drawn around one generator point: the shape a
/// neighbourhood-sized division is at a world extent, which has no interior tile and one
/// boundary cell. Overture's part 0 has three whose one place the build found no row for
/// (2026-08-29), and this is the fixture that says whether the resolution or the data is why.
const SPECK_HALF: f64 = 0.001;
/// How many tiles the fixture draws boxes over. Enough that the layer is a real minority of the
/// grid — the shape a boundary set actually has — and few enough to stay a fast test.
const BOXES: usize = 8;
/// The tile depth the narrow viewports are asked at. **A request's tiles come from its own `zoom`**,
/// so at zoom 0 a bbox resolves to the one tile covering the whole grid and narrows nothing — a
/// viewport case asked there would compare the whole map with itself.
const VIEWPORT_ZOOM: u8 = 4;

fn corpus() -> Corpus {
    Corpus::new(SEED, N, extent()).expect("the generator accepts the fixture's extent")
}

fn credential(grant: &str) -> Vec<u8> {
    let terms: Vec<String> = Grant::parse(grant)
        .expect("the grant is inside the generator's term space")
        .terms()
        .iter()
        .map(|t| format!("\"{}\"", t.raw()))
        .collect();
    format!("{{\"terms\": [{}]}}", terms.join(", ")).into_bytes()
}

/// The depth-[`DEPTH`] tile one position lands in — **the generator's own quantisation**, which is
/// the build's too: the same `cell`/`interleave_bits` pair, so the oracle and the engine cannot
/// disagree about which tile a point is in.
fn tile_of(x: f64, y: f64) -> u64 {
    let e = extent();
    let shift = 16 - u32::from(DEPTH);
    let cx = u32::from(tessera_spatial::cell(x, e.x_min, e.x_max)) >> shift;
    let cy = u32::from(tessera_spatial::cell(y, e.y_min, e.y_max)) >> shift;
    tessera_spatial::interleave_bits(cx, cy, DEPTH)
}

/// The middle half of tile `prefix`, as a box in extent coordinates.
///
/// **The middle half rather than the whole tile**, so two boxes never touch: the fixture's oracle
/// is per shape and a shared edge would put one point in two artifacts, which is legal but is not
/// what the counts below are asserting. The assertion beside every use is what makes that a fact
/// rather than an intention.
fn box_of(prefix: u64, tx: u32, ty: u32) -> [f64; 4] {
    let e = extent();
    let span = 65536u32 >> DEPTH;
    let at = |cells: u32, min: f64, max: f64| min + f64::from(cells) * (max - min) / 65536.0;
    let quarter = span / 4;
    let bbox = [
        at(tx * span + quarter, e.x_min, e.x_max),
        at(ty * span + quarter, e.y_min, e.y_max),
        at(tx * span + 3 * quarter, e.x_min, e.x_max),
        at(ty * span + 3 * quarter, e.y_min, e.y_max),
    ];
    let covering = tessera_spatial::tiles_for_bbox(bbox, DEPTH, &e);
    assert_eq!(
        covering.len(),
        1,
        "the fixture's box for prefix {prefix} covers {} tiles, not one",
        covering.len()
    );
    assert_eq!(covering[0].prefix, prefix, "and it covers the wrong one");
    bbox
}

/// The tiles this fixture draws boxes over, most populous first — so every artifact has members and
/// the counts below are numbers rather than zeros.
fn chosen_tiles(c: &Corpus) -> Vec<(u64, u32, u32)> {
    let mut population: BTreeMap<u64, u64> = BTreeMap::new();
    for e in 0..c.n() {
        let item = c.item(e);
        *population
            .entry(tile_of(item.x, item.y))
            .or_default() += 1;
    }
    let mut ranked: Vec<(u64, u64)> = population.into_iter().collect();
    ranked.sort_by_key(|(prefix, count)| (std::cmp::Reverse(*count), *prefix));
    ranked.truncate(BOXES);
    ranked.sort_by_key(|(prefix, _)| *prefix);

    // The `(tx, ty)` a prefix interleaves from, recovered by enumeration: the grid is `4^DEPTH`
    // tiles, which at this depth is a thousand-odd, and an inverse spread written by hand here
    // would be a second transcription of `interleave_bits`.
    let side = 1u32 << DEPTH;
    ranked
        .into_iter()
        .map(|(prefix, _)| {
            for ty in 0..side {
                for tx in 0..side {
                    if tessera_spatial::interleave_bits(tx, ty, DEPTH) == prefix {
                        return (prefix, tx, ty);
                    }
                }
            }
            panic!("prefix {prefix} is not a depth-{DEPTH} tile")
        })
        .collect()
}

/// The canonical shape one declaration produces — what the oracle tests a quantised point against.
fn canonical(shape: tessera_spatial::shape::ShapeF64) -> tessera_spatial::shape::Shape {
    shape
        .canonical(Space::View, &extent())
        .expect("the fixture's shapes canonicalise")
        .0
}

fn canonical_box(bbox: [f64; 4]) -> tessera_spatial::shape::Shape {
    canonical(tessera_spatial::shape::ShapeF64::Bbox {
        min_x: bbox[0],
        min_y: bbox[1],
        max_x: bbox[2],
        max_y: bbox[3],
    })
}

fn canonical_wkt(wkt: &str) -> tessera_spatial::shape::Shape {
    canonical(tessera_spatial::shape::ShapeF64::Polygon(
        tessera_spatial::shape::read_wkt(wkt).expect("the fixture's WKT reads"),
    ))
}

/// A point's grid position — **the build's own quantisation**, `fixed32` over the same extent,
/// so the oracle and the engine cannot disagree about where a point is.
fn grid_of(x: f64, y: f64) -> (u32, u32) {
    let e = extent();
    (
        tessera_spatial::fixed32(x, e.x_min, e.x_max),
        tessera_spatial::fixed32(y, e.y_min, e.y_max),
    )
}

/// The declaration: a box layer and a polygon layer, their shapes written into the document.
fn config_toml(c: &Corpus, criterion: &str) -> String {
    let artifacts: Vec<String> = chosen_tiles(c)
        .into_iter()
        .map(|(prefix, tx, ty)| {
            let bbox = box_of(prefix, tx, ty);
            format!(
                "  {{ key = \"t{prefix}\", bbox = [{}, {}, {}, {}] }},",
                bbox[0], bbox[1], bbox[2], bbox[3]
            )
        })
        .collect();
    format!(
        r#"
[sources]
points = "points.parquet"
pairs  = "pairs.parquet"

[[view]]
name             = "s0"
extent           = {{ min = 0.0, max = 1000.0 }}
point_visibility = {{ source = "pairs", default = "public" }}

[[layer]]
name                      = "{LAYER}"
views                     = ["s0"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = {criterion}
artifacts = [
{}
]

  [layer.shape]
  kind = "bbox"

[[layer]]
name                      = "{POLYGONS}"
views                     = ["s0"]
membership                = "spatial"
hierarchy                 = {{ kind = "flat" }}
visibility                = "public"
artifact_visibility       = {{ default = "inherited" }}
require_member_visibility = {criterion}
artifacts = [
  {{ key = "diamond", wkt = "{DIAMOND}" }},
  {{ key = "frame", wkt = "{FRAME}" }},
  {{ key = "speck", wkt = "{}" }},
]

  [layer.shape]
  kind = "polygon"
"#,
        artifacts.join("\n"),
        speck_wkt(c)
    )
}

/// The speck: a diamond of half-width [`SPECK_HALF`] around the corpus's first point.
fn speck_wkt(c: &Corpus) -> String {
    let item = c.item(0);
    let (x, y) = (item.x, item.y);
    let h = SPECK_HALF;
    format!(
        "POLYGON (({} {}, {} {}, {} {}, {} {}, {} {}))",
        x - h, y, x, y - h, x + h, y, x, y + h, x - h, y
    )
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    cache: std::path::PathBuf,
    wal: std::path::PathBuf,
    corpus: Corpus,
}

fn fixture(criterion: &str) -> Fixture {
    fixture_with(criterion, |toml| toml)
}

/// As [`fixture`], with the declaration edited on its way to the build — a pin, say.
fn fixture_with(criterion: &str, edit: impl FnOnce(String) -> String) -> Fixture {
    let corpus = corpus();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().join("bundle");
    let points = tmp.path().join("points.parquet");
    let pairs = tmp.path().join("pairs.parquet");
    corpus.write_points_parquet(&points).expect("points");
    corpus.write_pairs_parquet(&pairs).expect("pairs");
    let config_path = tmp.path().join("spatial-config.toml");
    std::fs::write(&config_path, edit(config_toml(&corpus, criterion))).unwrap();
    let config = tessera_build::config::Config::parse(&config_path, &Default::default())
        .expect("the spatial fixture's declaration parses");
    build_with_layers(&root, &points, &pairs, &corpus, config);
    let cache = tmp.path().join("cache");
    let wal = tmp.path().join("wal");
    Fixture {
        _tmp: tmp,
        root,
        cache,
        wal,
        corpus,
    }
}

impl Fixture {
    fn open(&self) -> Engine {
        let engine = open_engine_publishing(&self.root, &self.cache, &self.wal);
        engine.set_background_refresh_for_test(false);
        engine
    }

    /// The shapes, canonical, keyed as the layers key them — both layers in one map, the keys
    /// being disjoint.
    fn shapes(&self) -> Vec<(String, tessera_spatial::shape::Shape)> {
        let mut out: Vec<(String, tessera_spatial::shape::Shape)> = chosen_tiles(&self.corpus)
            .into_iter()
            .map(|(prefix, tx, ty)| (format!("t{prefix}"), canonical_box(box_of(prefix, tx, ty))))
            .collect();
        out.push(("diamond".to_string(), canonical_wkt(DIAMOND)));
        out.push(("frame".to_string(), canonical_wkt(FRAME)));
        out.push(("speck".to_string(), canonical_wkt(&speck_wkt(&self.corpus))));
        out
    }

    /// The oracle: every shape's masked count, computed from the generator and the shape's direct
    /// test alone. `extra` is points ingested since the build, in extent coordinates and visible to
    /// `grant`; `deleted` is the entities a change has taken away — the same adjustments the
    /// artifact census makes, so the expectation tracks the write cycle rather than only the build.
    fn expected(&self, grant: &str, deleted: &[u64], extra: &[(f64, f64)]) -> BTreeMap<String, u64> {
        let g = Grant::parse(grant).unwrap();
        let shapes = self.shapes();
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        let mut positions: Vec<(u32, u32)> = Vec::new();
        for e in 0..self.corpus.n() {
            if deleted.contains(&e) || !self.corpus.visible(e, &g) {
                continue;
            }
            let item = self.corpus.item(e);
            positions.push(grid_of(item.x, item.y));
        }
        for (x, y) in extra {
            positions.push(grid_of(*x, *y));
        }
        for (key, shape) in &shapes {
            let count = positions.iter().filter(|p| shape.contains(**p)).count() as u64;
            if count > 0 {
                counts.insert(key.clone(), count);
            }
        }
        counts
    }
}

/// What the layer serves one principal at one viewport, by key.
///
/// **`zoom` is not decoration.** A request's tiles come from `tiles_for_bbox(bbox, zoom, …)`, so at
/// zoom 0 every box resolves to the single tile covering the whole grid and a narrower `bbox`
/// narrows nothing. Every case below that is *about* the viewport therefore asks at a depth where
/// the box means something.
fn served(engine: &Engine, grant: &str, zoom: u8, bbox: [f64; 4]) -> BTreeMap<String, u64> {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [LAYER, POLYGONS];
    let mut request = ViewportRequest::new("s0", zoom, bbox, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
    engine
        .viewport(&session, request)
        .expect("a viewport over the fixture")
        .artifacts
        .into_iter()
        .map(|artifact| {
            (
                artifact.key.expect("the boxes are keyed"),
                artifact.masked_count,
            )
        })
        .collect()
}

fn fold(engine: &Engine) {
    let before = engine.write_executor_stats();
    engine.request_fold();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
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

/// Ingest one point at `(x, y)` visible to `term`.
fn ingest_point(engine: &Engine, external_id: &str, term: u32, x: f64, y: f64) {
    let descriptors = vec![term.to_string().into_bytes()];
    let mut hash = [0u8; 32];
    for (slot, byte) in hash.iter_mut().zip(external_id.as_bytes()) {
        *slot = *byte;
    }
    engine
        .accept_ingest(
            vec![UnallocatedRow {
                external_id: Some(external_id.as_bytes().to_vec()),
                view: "s0".to_string(),
                descriptors: descriptors.clone(),
                x,
                y,
                scalars: Vec::new(),
                terms: engine.resolve_terms(&descriptors),
            }],
            external_id.to_string(),
            hash,
        )
        .expect("an ordinary point is an ordinary write");
}

// ---------------------------------------------------------------------------------------------

/// **The counts are the generator's**, for every principal and every viewport.
///
/// A count is over the whole membership rather than over the viewport (`annotations.md` §4.2), so
/// panning must move which artifacts are served and never the number beside one — which is asserted
/// here as well as the counts themselves, because a count that narrowed with the box would let a
/// viewer difference two boxes for the members in between.
#[test]
fn a_boundarys_masked_count_is_the_one_the_generator_computes() {
    let fx = fixture("\"none\"");
    let engine = fx.open();

    for grant in ["0", "0,1", "1,2,3", "5,6,7,8"] {
        let expected = fx.expected(grant, &[], &[]);
        assert_eq!(
            served(&engine, grant, 0, WHOLE_MAP),
            expected,
            "the served counts differ from the generator's for grant {grant}"
        );
        assert!(
            expected.contains_key("diamond") && expected.contains_key("frame"),
            "grant {grant} sees no polygon member at all, so the polygon layer is untested"
        );

        // Every narrower viewport serves a subset, at the *same* counts.
        for bbox in [
            [0.0, 0.0, 500.0, 500.0],
            [250.0, 250.0, 750.0, 750.0],
            [0.0, 0.0, 125.0, 125.0],
        ] {
            let narrow = served(&engine, grant, VIEWPORT_ZOOM, bbox);
            for (key, count) in &narrow {
                assert_eq!(
                    expected.get(key),
                    Some(count),
                    "{key}'s count moved with the viewport under grant {grant}"
                );
            }
            assert!(narrow.len() <= expected.len());
        }
    }

    // A viewport in a corner serves strictly fewer shapes than the whole map, or the candidacy
    // arithmetic is not being exercised at all.
    let whole = served(&engine, "0,1", 0, WHOLE_MAP);
    let corner = served(&engine, "0,1", VIEWPORT_ZOOM, [0.0, 0.0, 125.0, 125.0]);
    assert!(
        corner.len() < whole.len(),
        "every shape is a candidate in a corner viewport, so candidacy decides nothing"
    );
}

/// **A point ingested inside a boundary is a member on the next request, with nothing rebuilt.**
///
/// Nothing rebuilt is stated as what did not happen: no fold, and no write to the layer at all —
/// the flush resolved its own segment against the shapes before it published
/// (`polygon-membership.md` §6.3), and the next request joined a segment list that includes it.
/// That is the property a stored membership cannot have, and it is why the source exists. Two
/// points, so the polygon layer's boundary test is exercised as well as the box's: one at the
/// centre of a box, one just inside the diamond's edge — and one just outside it, which must
/// count nowhere.
#[test]
fn a_point_ingested_inside_a_boundary_counts_on_the_next_request() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let before = served(&engine, "0", 0, WHOLE_MAP);
    // The box this principal sees most of — the tile with the most points is not necessarily
    // the box with the most visible ones, the box being the tile's middle half.
    let (key, bbox) = chosen_tiles(&fx.corpus)
        .into_iter()
        .map(|(prefix, tx, ty)| (format!("t{prefix}"), box_of(prefix, tx, ty)))
        .max_by_key(|(key, _)| before.get(key).copied().unwrap_or(0))
        .expect("the fixture draws boxes");
    assert!(before.contains_key(&key), "no box has a visible member");
    let folds = engine.write_executor_stats().folds;

    // The centre of the box; a point 2 units inside the diamond's north-east edge, on which
    // `x + y = 1400` lies; and one 2 units outside it.
    let inside = [
        ((bbox[0] + bbox[2]) / 2.0, (bbox[1] + bbox[3]) / 2.0),
        (700.0, 698.0),
    ];
    let outside = (700.0, 702.0);
    ingest_point(&engine, "inside-1", 0, inside[0].0, inside[0].1);
    ingest_point(&engine, "inside-2", 0, inside[1].0, inside[1].1);
    ingest_point(&engine, "outside-1", 0, outside.0, outside.1);
    flush(&engine);

    let after = served(&engine, "0", 0, WHOLE_MAP);
    let mut extra: Vec<(f64, f64)> = inside.to_vec();
    extra.push(outside);
    assert_eq!(
        after,
        fx.expected("0", &[], &extra),
        "the ingested points did not reach exactly the shapes they landed in"
    );
    assert_eq!(after[&key], before[&key] + 1);
    assert_eq!(after["diamond"], before["diamond"] + 1);
    assert_eq!(
        engine.write_executor_stats().folds,
        folds,
        "the count moved because of a fold rather than because of the geometry"
    );
}

/// **A fold leaves every answer where it was.** A fold renumbers the base row space wholesale,
/// which is exactly what a stored membership has to be rewritten for; a shape's membership is
/// re-derived over the rows the fold wrote, so nothing about it is a projection that could go
/// stale.
#[test]
fn a_fold_leaves_a_boundarys_answers_unchanged() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let grant = "0,1";
    let before = served(&engine, grant, 0, WHOLE_MAP);
    let before_narrow = served(&engine, grant, VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]);
    assert!(!before.is_empty());

    // Something to fold: a flushed extent above the base, so the fold has rows to renumber.
    ingest_point(&engine, "pre-fold", 0, 3.0, 3.0);
    flush(&engine);
    fold(&engine);

    let after = served(&engine, grant, 0, WHOLE_MAP);
    let after_narrow = served(&engine, grant, VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]);
    for (key, count) in &before {
        // The ingested point may itself have landed in one of the boxes, which is a real +1 rather
        // than a discrepancy — so the assertion is that nothing *lost* members.
        assert!(
            after.get(key).is_some_and(|now| now >= count),
            "{key} lost members across a fold: {count} → {:?}",
            after.get(key)
        );
    }
    assert_eq!(after.len(), before.len(), "a box left the map at the fold");
    assert_eq!(after_narrow.len(), before_narrow.len());
}

/// **A suppressed member leaves the count, and a suppressed artifact leaves the map** — the same
/// two rules a stored membership follows, reached through a membership nothing stores.
#[test]
fn a_deny_reaches_a_boundary_and_its_members() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let grant = "0,1";
    let before = served(&engine, grant, 0, WHOLE_MAP);
    // The shape this principal sees most of, from either layer — the boxes are small and a
    // narrow principal may see one or two members of each; the diamond covers a third of the map.
    let (key, shape) = fx
        .shapes()
        .into_iter()
        .max_by_key(|(key, _)| before.get(key).copied().unwrap_or(0))
        .expect("the fixture draws shapes");
    let was = before[&key];
    assert!(was > 2, "the fullest shape has too few members to test with");

    // One visible member of that shape, suppressed.
    let by_source = source_to_new_map(&fx.root, "v00000");
    let g = Grant::parse(grant).unwrap();
    let victim = (0..fx.corpus.n())
        .find(|e| {
            let item = fx.corpus.item(*e);
            fx.corpus.visible(*e, &g) && shape.contains(grid_of(item.x, item.y))
        })
        .expect("the box has a visible member");
    engine
        .accept_change(EntityId::new(by_source[&victim]), ChangeOp::Suppress)
        .expect("a suppression is accepted");

    assert_eq!(
        served(&engine, grant, 0, WHOLE_MAP)[&key],
        was - 1,
        "the suppressed member is still counted inside its boundary"
    );

    // And the artifact's own entity: suppressed, it is absent whole.
    let id = served_id(&engine, grant, &key);
    engine
        .accept_change(id, ChangeOp::Suppress)
        .expect("an artifact's own suppression is accepted");
    assert!(
        !served(&engine, grant, 0, WHOLE_MAP).contains_key(&key),
        "a suppressed boundary is still on the map"
    );
}

/// The entity behind a served artifact, found by asking the identifier route to resolve what the
/// viewport handed out — the same address a suppression names.
fn served_id(engine: &Engine, grant: &str, key: &str) -> EntityId {
    let session = engine.authorise(&credential(grant)).unwrap();
    let names = [LAYER, POLYGONS];
    let mut request = ViewportRequest::new("s0", 0, WHOLE_MAP, N as usize);
    request.layers = tessera_engine::LayerSelection::Named(&names);
    let row = engine
        .viewport(&session, request)
        .expect("a viewport")
        .artifacts
        .into_iter()
        .find(|artifact| artifact.key.as_deref() == Some(key))
        .expect("the key is served");
    // Inverted through the admin plane's own resolver, the way `/control/changes` does — so the
    // suppression below exercises the misdirection guard rather than going round it.
    let idset = engine.generation().bundle.manifest.identity.idset;
    engine
        .resolve_tessera_ids(&[row.tessera_id], idset)
        .unwrap()[0]
        .expect("an artifact identifier names the entity this deployment issued for it")
}

/// **An absolute criterion fires on a shape exactly as it does on a stored set.** The number tested
/// is the number served — one quantity, computed once — so a box a viewer can see too little of is
/// absent rather than served with a small count beside it.
#[test]
fn an_absolute_criterion_fires_on_a_boundary() {
    let fx = fixture("\"none\"");
    let engine = fx.open();
    let open = served(&engine, "0,1", 0, WHOLE_MAP);
    // **The median of what is served**, so the bar withholds roughly half: a bar under every count
    // would leave the case asserting that a criterion changes nothing.
    let mut counts: Vec<u64> = open.values().copied().collect();
    counts.sort_unstable();
    let bar = counts[counts.len() / 2];
    assert!(bar > 0, "the fixture's counts are too small to set a bar");
    drop(engine);

    let gated = fixture(&format!("{{ count = {bar} }}"));
    let engine = gated.open();
    let served_gated = served(&engine, "0,1", 0, WHOLE_MAP);
    let expected: BTreeMap<String, u64> = gated
        .expected("0,1", &[], &[])
        .into_iter()
        .filter(|(_, count)| *count >= bar)
        .collect();
    assert_eq!(
        served_gated, expected,
        "the criterion tested a different number from the one served"
    );
    assert!(
        served_gated.len() < open.len(),
        "the criterion withheld nothing, so it is untested"
    );
}

/// **A shape survives a restart**, which is the one thing about a spatial layer that is durable.
///
/// The memberships are not stored — every segment is resolved again at open against the shapes
/// the records carry — so there is nothing about them a restart could lose. What *is* stored is
/// the canonical shape itself, in the artifact's own record blob, and an artifact restored without
/// one has no membership rule at all: it would count zero for every viewer and be absent under any
/// criterion, which no client can tell from an artifact whose members are simply invisible to
/// them. So the decoder refuses such a blob, and this asserts the whole path — canonicalise and
/// encode at publication, map at open, decode, decompose, resolve — by asking for the same answers
/// twice across a reopen.
#[test]
fn a_boundarys_box_survives_a_restart() {
    let fx = fixture("\"none\"");
    let (before, before_narrow) = {
        let engine = fx.open();
        (
            served(&engine, "0,1", 0, WHOLE_MAP),
            served(&engine, "0,1", VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]),
        )
    };
    assert!(!before.is_empty());

    let engine = fx.open();
    assert_eq!(
        served(&engine, "0,1", 0, WHOLE_MAP),
        before,
        "a boundary's shape did not come back from the extent it was written into"
    );
    assert_eq!(
        served(&engine, "0,1", VIEWPORT_ZOOM, [0.0, 0.0, 500.0, 500.0]),
        before_narrow
    );

    // And again after a fold, which rewrites every extent whole — the path a restored box takes
    // through `repack_all` rather than through the publication that first wrote it.
    ingest_point(&engine, "pre-fold", 0, 3.0, 3.0);
    flush(&engine);
    fold(&engine);
    drop(engine);

    let engine = fx.open();
    let after = served(&engine, "0,1", 0, WHOLE_MAP);
    assert_eq!(
        after.len(),
        before.len(),
        "a shape was lost by the fold's rewrite"
    );
    for (key, count) in &before {
        assert!(
            after.get(key).is_some_and(|now| now >= count),
            "{key} lost members across a fold and a restart"
        );
    }
}

/// **An open claims what the build and the fold persisted, and resolves only the segments no
/// persisted form covers** (`polygon-membership.md` §6.3; owner ruling 2026-08-29).
///
/// The build writes each spatial level's resolved rows in its layout's form — the row-major column
/// under a `column` pin, the `shape-rows` row form otherwise — and every fold writes them again.
/// An open reads those rather than resolving the base segment; a flushed segment has no form and
/// is resolved. Both the cadence and the answers are asserted: the counts against the generator's
/// oracle at every stage, so a claimed piece is the same membership the resolution would have
/// produced.
#[test]
fn an_open_claims_the_persisted_pieces_and_resolves_only_the_flushed_segments() {
    // The boxes pinned `column` so one level is claimed from its column and the other from its
    // row form; the boxes never touch, so the column composes.
    let fx = fixture_with("\"none\"", |toml| {
        toml.replacen(
            "membership                = \"spatial\"\n",
            "membership                = \"spatial\"\nlayout                    = \"column\"\n",
            1,
        )
    });
    let engine = fx.open();
    assert_eq!(served(&engine, "0,1", 0, WHOLE_MAP), fx.expected("0,1", &[], &[]));
    let warm = engine.shape_warm_report();
    assert_eq!(warm.levels, 2, "two spatial layers, one level each, one view");
    assert_eq!(
        (warm.pieces_claimed, warm.pieces_resolved),
        (2, 0),
        "a fresh open of a built bundle claims both levels' persisted forms: {warm:?}"
    );
    assert_eq!(
        (warm.held_claimed, warm.held_decomposed),
        (warm.artifacts, 0),
        "every decomposition is claimed from the build's file, none descended: {warm:?}"
    );

    // A flush adds a segment nothing persisted covers: claimed 2, resolved 2, and the ingested
    // point counts.
    ingest_point(&engine, "flushed", 0, 3.0, 3.0);
    flush(&engine);
    drop(engine);
    let engine = fx.open();
    let warm = engine.shape_warm_report();
    assert_eq!(
        (warm.pieces_claimed, warm.pieces_resolved),
        (2, 2),
        "the base is claimed and the flushed segment resolved: {warm:?}"
    );
    assert_eq!(
        served(&engine, "0,1", 0, WHOLE_MAP),
        fx.expected("0,1", &[], &[(3.0, 3.0)])
    );

    // A fold renumbers every row and writes the forms again under the new prefix: claimed 2,
    // resolved 0.
    fold(&engine);
    drop(engine);
    let engine = fx.open();
    let warm = engine.shape_warm_report();
    assert_eq!(
        (warm.pieces_claimed, warm.pieces_resolved),
        (2, 0),
        "a fresh open after a fold claims the fold's forms: {warm:?}"
    );
    assert_eq!(
        (warm.held_claimed, warm.held_decomposed),
        (warm.artifacts, 0),
        "the fold wrote the decompositions again: {warm:?}"
    );
    assert_eq!(
        served(&engine, "0,1", 0, WHOLE_MAP),
        fx.expected("0,1", &[], &[(3.0, 3.0)])
    );
    drop(engine);

    // A stale or torn file is refused and the segment resolved again — loudly, never served
    // short. Truncate the fold's row form and reopen: one level claimed (the column), one
    // resolved, every answer unchanged.
    let forms: Vec<std::path::PathBuf> = walk(&fx.root)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "tssr"))
        .collect();
    assert_eq!(forms.len(), 1, "one row form for the polygon level: {forms:?}");
    let bytes = std::fs::read(&forms[0]).unwrap();
    std::fs::write(&forms[0], &bytes[..bytes.len() / 2]).unwrap();
    let engine = fx.open();
    let warm = engine.shape_warm_report();
    assert_eq!(
        (warm.pieces_claimed, warm.pieces_resolved),
        (1, 1),
        "a torn form is refused and its segment resolved: {warm:?}"
    );
    assert_eq!(
        served(&engine, "0,1", 0, WHOLE_MAP),
        fx.expected("0,1", &[], &[(3.0, 3.0)])
    );
}

fn walk(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}
